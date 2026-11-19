//! The simulated camera (docs/AUTO-CONTROLS.md 6.1): a discrete-event model driven in 1 ms steps that produces
//! `Stats` for the controller and executes its writes with realistic delays. Tests only.
//!
//! Scene: a 16x16 reflectance map (grey wall, face-like mid patch, two bright cells, dark corner), a scripted light
//! `L(t)` (a linear factor), a scripted light temperature `T(t)` (Kelvin) and optional mains flicker. Light needed is
//! stated in stops: with `L = 2^(-need)` the picture is on target when `E + gs(G) = need` (the scale is calibrated
//! numerically at construction for target 0.45).
//!
//! White balance (`SimCfg::wb`): the picture's blue-red signal is `c = α_true·(M_set − M_light)` in `ln(B/R)`, colours
//! normalised to the luma of the grey, so the exposure meter does not see the white balance. A linear model in
//! mired: the doc's three-point table is only a way to make such a signal; this is the signal.
//!
//! Coloured scenes (`SimCfg::scene`, docs/AUTO-WHITE-BALANCE.md 5.2, W3): a scene of the kit `fx::awb::scenes` (an
//! office, a grey room, a face filling the frame, ...) replaces the grey wall. Each cell keeps its reflectance
//! colour, and the cast is `(bb(T_light) / bb(K_set))^(α_true / 0.0105)` per channel (a blackbody ratio, so
//! `α_true = 0.0105` is the physical camera and the `α_true` variants stay). Each cell is normalised to its own luma,
//! so the exposure meter still does not see the colour. The true `e_wb` is the blue-red signal of that cast (what a
//! neutral surface shows) minus the aim.
//!
//! Faults (6.1) are public fields of [`Faults`], set by the test at any time: `ignore` (accepted, the read-back
//! shows the old value), `no_effect` (the read-back shows the new value, the picture stays), `fail` (every write and
//! read errors), `slow` (every write after the first takes this long), `meddles` (another program writes).

use std::collections::VecDeque;

use super::meter::{measure_exposure, srgb_decode, srgb_encode, warmth_target, GRID};
use super::*;
use crate::capture::controls::AccessPath;
use crate::fx::awb::{real, scenes};

pub type Light = Box<dyn Fn(f64) -> f64>;

/// A scene `need` stops dark (`E + gs(G) = need` puts it on target), constant.
pub fn dim(need: f64) -> Light {
    let l = (-need).exp2();
    Box::new(move |_| l)
}

/// A constant light temperature.
pub fn kelvin(k: f64) -> Light {
    Box::new(move |_| k)
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

/// The camera has a manual white balance.
#[derive(Clone, Copy, Debug)]
pub struct WbSim {
    /// `c` per mired between the setting and the light.
    pub alpha_true: f64,
    pub start_k: i32,
    /// The camera has a Camera auto flag for white balance (where a hand-back goes).
    pub can_auto: bool,
}

impl Default for WbSim {
    fn default() -> Self {
        WbSim {
            alpha_true: 0.008,
            start_k: 4600,
            can_auto: true,
        }
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
    /// The camera lowers its rate when `tau > rate_drop_fill / F`.
    pub rate_drop: bool,
    /// The part of the frame time an exposure may fill before the camera slows down (W4GS at 60 fps: 0.86).
    pub rate_drop_fill: f64,
    /// The camera never exceeds this rate, whatever the exposure.
    pub fps_cap: Option<f64>,
    /// The first write to exposure (and white balance) after "open" takes 0.5 s.
    pub slow_first_write: bool,
    pub flicker: Option<(f64, f64)>,
    /// Standard deviation of the per-frame light jitter in stops.
    pub jitter: f64,
    /// `stale_auto`: reads report this exposure and the Auto flag on.
    pub stale_exposure: Option<i32>,
    pub start_e: i32,
    pub start_g: i32,
    pub seed: u64,
    pub wb: Option<WbSim>,
    /// The camera has Camera auto for exposure.
    pub exposure_auto: bool,
    /// A coloured scene of the kit `fx::awb::scenes` (`None`: the grey wall).
    pub scene: Option<&'static str>,
    /// Frames recorded from a real camera replace the scene (white balance only: the exposure meter is not fed).
    pub replay: Option<Replay>,
}

/// Recorded frames (`fx::awb::real`) played to the controller, forward and back so the loop has no cut.
#[derive(Clone)]
pub struct Replay {
    frames: std::sync::Arc<Vec<real::Frame>>,
}

impl Replay {
    pub fn new(frames: Vec<real::Frame>) -> Replay {
        assert!(frames.len() > 1, "a replay needs frames");
        Replay { frames: std::sync::Arc::new(frames) }
    }

    /// The picture at sim time `t` that a camera at `k` would have made (`alpha_true` colour signal per mired): the
    /// recorded frame, its cells moved along the blue-red axis by the difference between `k` and the Kelvin the
    /// camera had when it was recorded, each cell keeping its luma.
    fn shot(&self, t: f64, k: i32, alpha_true: f64) -> Stats {
        let (t0, t1) = (self.frames[0].t, self.frames[self.frames.len() - 1].t);
        let span = t1 - t0;
        let phase = t.rem_euclid(2.0 * span);
        let pos = t0 + if phase < span { phase } else { 2.0 * span - phase };
        let at = self.frames.partition_point(|f| f.t <= pos).saturating_sub(1);
        let f = &self.frames[at];
        let dc = alpha_true * (1e6 / f64::from(k) - 1e6 / f64::from(f.k));
        let m = [(-0.5 * dc).exp(), (LOCUS_SLOPE * dc).exp(), (0.5 * dc).exp()];
        let cells = f
            .stats
            .cells
            .iter()
            .map(|c| {
                let rgb = [0, 1, 2].map(|i| f64::from(c[i]));
                let moved = [0, 1, 2].map(|i| rgb[i] * m[i]);
                let n = (0.2126 * rgb[0] + 0.7152 * rgb[1] + 0.0722 * rgb[2])
                    / (0.2126 * moved[0] + 0.7152 * moved[1] + 0.0722 * moved[2]).max(1e-9);
                let v = [0, 1, 2].map(|i| (moved[i] * n).min(1.0) as f32);
                [v[0], v[1], v[2], c[3]]
            })
            .collect();
        Stats { cells }
    }
}

impl Default for SimCfg {
    fn default() -> Self {
        SimCfg {
            fps: 30.0,
            gain: true,
            s_true: 3.0,
            bent: false,
            rate_drop: false,
            rate_drop_fill: 0.8,
            fps_cap: None,
            slow_first_write: true,
            flicker: None,
            jitter: 0.0,
            stale_exposure: None,
            start_e: -6,
            start_g: 0,
            seed: 7,
            wb: None,
            exposure_auto: true,
            scene: None,
            replay: None,
        }
    }
}

/// Another program's write.
#[derive(Clone, Copy, Debug)]
pub struct Meddle {
    pub at: f64,
    pub key: ControlKey,
    pub value: i32,
}

/// The fault switches of 6.1.
#[derive(Clone, Debug, Default)]
pub struct Faults {
    /// Writes to these controls are accepted and have no effect; the read-back shows the old value.
    pub ignore: Vec<ControlKey>,
    /// Writes to these controls are accepted and read back, but the picture does not change.
    pub no_effect: Vec<ControlKey>,
    /// Every write and read errors.
    pub fail: bool,
    /// Every write after the first takes this long (seconds).
    pub slow: Option<f64>,
    pub meddles: Vec<Meddle>,
    /// A write that never returns.
    pub hang: bool,
}

const GAIN_MAX: i32 = 100;
pub const WB_MIN: i32 = 2800;
pub const WB_MAX: i32 = 6500;
const WRITE_S: f64 = 0.005;
const FIRST_WRITE_S: f64 = 0.5;
/// A write lands at the first frame starting this long after it returned.
const LAND_S: f64 = 0.1;
/// A measurement is submitted at most this often (the engine's rate).
const SUBMIT_EVERY: f64 = 0.066;
const SUB: usize = 16;

/// The tint of a real light: green over the mean of red and blue is `e^(-0.14 c)` for a blue over red of `e^c` (the
/// blackbody locus, `fx::awb`'s line). A cast with no green shift would sit off the locus, on the side the skin guard
/// reads as skin, and a strong one would be held for that.
const LOCUS_SLOPE: f64 = -0.14;

/// The channel multipliers of a picture whose blue over red is `e^c`, on the locus, with the luma of the grey kept
/// at 1.
fn tint(c: f64) -> [f64; 3] {
    let (r, g, b) = ((-0.5 * c).exp(), (LOCUS_SLOPE * c).exp(), (0.5 * c).exp());
    let n = 1.0 / (0.2126 * r + 0.7152 * g + 0.0722 * b);
    [r * n, g * n, b * n]
}

/// The scene: reflectances, in-cell texture and the calibrated scale.
struct Scene {
    /// The brightness of each cell (its luma), what the exposure meter sees.
    refl: Vec<f64>,
    /// The colour of each cell (the reflectance; white for the grey wall).
    rho: Vec<[f64; 3]>,
    tex: [f64; SUB],
    scale: f64,
    s_true: f64,
    bent: bool,
}

impl Scene {
    fn new(s_true: f64, bent: bool, kit: Option<&str>) -> Scene {
        let mut refl = vec![1.0; GRID * GRID];
        let mut rho = vec![[1.0; 3]; GRID * GRID];
        if let Some(name) = kit {
            for (i, c) in scenes::scene(name).iter().enumerate() {
                rho[i] = c.rho;
                refl[i] = scenes::luma(c.rho);
            }
            for i in scenes::HIGHLIGHTS {
                rho[i] = [1.0; 3]; // a lamp or a specular spot: white, clipped
                refl[i] = 2.0;
            }
        } else {
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
        }
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
            rho,
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
            let e = measure_exposure(&scene.render(0, 0, 1.0, None, None), 0.45).error;
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

    /// The statistics image of a frame exposed with `2^e` s, gain setting `g`, at light factor `l`, under the colour
    /// cast `cast` (per channel; `None` is neutral). Each cell keeps its own luma.
    fn render(&self, e: i32, g: i32, l: f64, cast: Option<[f64; 3]>, mut rng: Option<&mut Rng>) -> Stats {
        let gain = self.gs(g).exp2();
        let base = self.scale * l * f64::from(e).exp2() * gain;
        let mut cells = Vec::with_capacity(GRID * GRID);
        for (r, rho) in self.refl.iter().zip(&self.rho) {
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
            let m = sum / SUB as f64;
            cells.push(match cast {
                None => [m as f32, m as f32, m as f32, 1.0],
                Some(c) => {
                    // The cell's colour under the cast, scaled back to the cell's own luma.
                    let f = [rho[0] * c[0], rho[1] * c[1], rho[2] * c[2]];
                    let n = m / (0.2126 * f[0] + 0.7152 * f[1] + 0.0722 * f[2]);
                    [
                        (f[0] * n).min(1.0) as f32,
                        (f[1] * n).min(1.0) as f32,
                        (f[2] * n).min(1.0) as f32,
                        1.0,
                    ]
                }
            });
        }
        Stats { cells }
    }
}

#[derive(Clone, Copy, Debug)]
struct Capture {
    t: f64,
    e: i32,
    g: i32,
    /// The white balance the frame was made with.
    k: i32,
    light: f64,
    cast: Option<[f64; 3]>,
}

#[derive(Clone, Copy, Debug)]
struct Landing {
    at: f64,
    key: ControlKey,
    value: i32,
    /// The picture changes with it (false for `no_effect`).
    effective: bool,
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
    /// True white balance error `e_wb` of the landed state (noiseless); 0 without white balance.
    pub wb_err: f64,
}

pub struct Sim {
    pub cfg: SimCfg,
    pub faults: Faults,
    scene: Scene,
    light: Light,
    temp: Light,
    rng: Rng,
    ms: u64,
    pub t: f64,
    /// What the camera reports.
    dev_e: i32,
    dev_g: i32,
    dev_k: i32,
    /// What the picture is made with (differs from the report under `no_effect`).
    eff_e: i32,
    eff_g: i32,
    eff_k: i32,
    landings: Vec<Landing>,
    queue: VecDeque<(ControlKey, i32)>,
    in_flight: Option<InFlight>,
    first_exposure_write: bool,
    first_wb_write: bool,
    next_frame: f64,
    last_submit: f64,
    /// Delivery times of frames.
    frames: Vec<f64>,
    deliveries: VecDeque<(f64, Capture)>,
    pub paused: bool,
    pub resumed_at: f64,
    /// Writes that went through.
    pub writes: Vec<WriteRec>,
    /// Every write the camera received, failed ones too.
    pub attempts: Vec<WriteRec>,
    /// Every `Write` action the controller returned, with the tick time.
    pub issued: Vec<(f64, ControlKey, i32)>,
    pub handbacks: Vec<(f64, ControlKey, HandBack)>,
    pub readbacks: usize,
    /// The statuses that went with each `Emit`.
    pub emitted: Vec<AutoStatus>,
    /// The GPU stopped delivering measurements (W5).
    pub no_samples: bool,
    pub history: Vec<Snap>,
    /// The highest exposure that was ever applied to a frame.
    pub max_e_applied: i32,
    pub emits: usize,
}

impl Sim {
    pub fn new(cfg: SimCfg, light: Light) -> Sim {
        let scene = Scene::new(cfg.s_true, cfg.bent, cfg.scene);
        let k0 = cfg.wb.map_or(4600, |w| w.start_k);
        Sim {
            rng: Rng::new(cfg.seed),
            dev_e: cfg.start_e,
            dev_g: cfg.start_g,
            dev_k: k0,
            eff_e: cfg.start_e,
            eff_g: cfg.start_g,
            eff_k: k0,
            max_e_applied: cfg.start_e,
            scene,
            light,
            temp: kelvin(5000.0),
            faults: Faults::default(),
            ms: 0,
            t: 0.0,
            landings: Vec::new(),
            queue: VecDeque::new(),
            in_flight: None,
            first_exposure_write: true,
            first_wb_write: true,
            next_frame: 0.0,
            last_submit: f64::NEG_INFINITY,
            frames: Vec::new(),
            deliveries: VecDeque::new(),
            paused: false,
            resumed_at: f64::NEG_INFINITY,
            writes: Vec::new(),
            attempts: Vec::new(),
            issued: Vec::new(),
            handbacks: Vec::new(),
            readbacks: 0,
            emitted: Vec::new(),
            no_samples: false,
            history: Vec::new(),
            emits: 0,
            cfg,
        }
    }

    /// The light's colour temperature over time (Kelvin).
    pub fn with_temperature(mut self, temp: Light) -> Sim {
        self.temp = temp;
        self
    }

    /// The `ControlInfo` of one control as `read_all` reports it (W4GS ranges). With `stale_exposure` the Auto flag
    /// is on and the value is the stale one.
    fn info(&self, key: ControlKey) -> Option<ControlInfo> {
        match key {
            ControlKey::Exposure => Some(ControlInfo {
                key,
                min: -13,
                max: -1,
                step: 1,
                default: -6,
                value: self.cfg.stale_exposure.unwrap_or(self.dev_e),
                can_auto: self.cfg.exposure_auto,
                can_manual: true,
                auto: self.cfg.stale_exposure.is_some(),
                path: AccessPath::Ks,
            }),
            ControlKey::Gain if self.cfg.gain => Some(ControlInfo {
                key,
                min: 0,
                max: GAIN_MAX,
                step: 1,
                default: 0,
                value: self.dev_g,
                can_auto: false,
                can_manual: true,
                auto: false,
                path: AccessPath::Ks,
            }),
            ControlKey::WhiteBalance => self.cfg.wb.map(|w| ControlInfo {
                key,
                min: WB_MIN,
                max: WB_MAX,
                step: 1,
                default: 4600,
                value: self.dev_k,
                can_auto: w.can_auto,
                can_manual: true,
                auto: false,
                path: AccessPath::Ks,
            }),
            _ => None,
        }
    }

    pub fn caps(&self) -> Vec<ControlInfo> {
        [ControlKey::Exposure, ControlKey::Gain, ControlKey::WhiteBalance]
            .into_iter()
            .filter_map(|k| self.info(k))
            .collect()
    }

    /// What the camera reports for each driven control.
    pub fn device(&self) -> (i32, i32) {
        (self.dev_e, self.dev_g)
    }

    pub fn device_k(&self) -> i32 {
        self.dev_k
    }

    /// The colour cast of a picture made with `k` under `t_light` Kelvin (`None` without white balance). A grey
    /// scene: a blue-red signal `α_true·(M_set − M_light)` with the luma kept. A kit scene: a blackbody ratio to the
    /// power `α_true / 0.0105`.
    fn cast(&self, k: i32, t_light: f64) -> Option<[f64; 3]> {
        let w = self.cfg.wb?;
        if self.cfg.scene.is_some() {
            let (light, set) = (scenes::blackbody_rgb(t_light), scenes::blackbody_rgb(f64::from(k)));
            let p = w.alpha_true / 0.0105;
            Some([0, 1, 2].map(|i| (light[i] / set[i]).powf(p)))
        } else {
            Some(tint(w.alpha_true * (1e6 / f64::from(k) - 1e6 / t_light)))
        }
    }

    fn oracle(&self, target: f64, warmth: f64) -> (meter::ExposureReading, f64) {
        let cast = self.cast(self.eff_k, (self.temp)(self.t));
        let stats = self
            .scene
            .render(self.eff_e, self.eff_g, (self.light)(self.t), cast, None);
        let wb = cast.map_or(0.0, |c| {
            // Not `measure_white_balance`: the true `e_wb` is the blue-red signal of the cast minus the aim.
            (c[2] / c[0]).ln() - warmth_target(warmth)
        });
        (measure_exposure(&stats, target), wb)
    }

    /// The noiseless statistics image of the landed state now: what Auto Adjust would measure after this loop.
    pub fn picture(&self) -> Stats {
        if let (Some(r), Some(w)) = (&self.cfg.replay, self.cfg.wb) {
            return r.shot(self.t, self.eff_k, w.alpha_true);
        }
        let cast = self.cast(self.eff_k, (self.temp)(self.t));
        self.scene
            .render(self.eff_e, self.eff_g, (self.light)(self.t), cast, None)
    }

    /// The colour a neutral surface has in the landed state now (the cast, linear RGB; white without white balance).
    pub fn neutral_now(&self) -> [f64; 3] {
        self.cast(self.eff_k, (self.temp)(self.t)).unwrap_or([1.0; 3])
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
            self.step_meddles();
            self.step_writes(ctrl);
            self.step_frames();
            self.step_deliveries(ctrl);
            if self.ms.is_multiple_of(100) {
                self.step_tick(ctrl);
            }
        }
    }

    /// Another program's writes land at the next frame.
    fn step_meddles(&mut self) {
        let t = self.t;
        let (due, later): (Vec<Meddle>, Vec<Meddle>) = std::mem::take(&mut self.faults.meddles)
            .into_iter()
            .partition(|m| m.at <= t);
        self.faults.meddles = later;
        for m in due {
            self.landings.push(Landing {
                at: t,
                key: m.key,
                value: m.value,
                effective: true,
            });
        }
    }

    fn step_writes(&mut self, ctrl: &mut AutoController) {
        if let Some(w) = self.in_flight {
            if self.t >= w.done && !self.faults.hang {
                self.in_flight = None;
                let rec = WriteRec {
                    started: w.started,
                    done: w.done,
                    key: w.key,
                    value: w.value,
                };
                self.attempts.push(rec);
                let reported = |s: &Sim| match w.key {
                    ControlKey::Exposure => s.dev_e,
                    ControlKey::Gain => s.dev_g,
                    _ => s.dev_k,
                };
                let (ok, readback) = if self.faults.fail {
                    (false, None)
                } else if self.faults.ignore.contains(&w.key) {
                    (true, Some(reported(self)))
                } else {
                    (true, Some(w.value))
                };
                if ok {
                    self.writes.push(rec);
                    if !self.faults.ignore.contains(&w.key) {
                        self.landings.push(Landing {
                            at: w.done + LAND_S,
                            key: w.key,
                            value: w.value,
                            effective: !self.faults.no_effect.contains(&w.key),
                        });
                    }
                }
                ctrl.on_write_done(WriteDone {
                    key: w.key,
                    requested: w.value,
                    readback,
                    ok,
                    started: w.started,
                    done: w.done,
                });
            }
        }
        if self.in_flight.is_none() {
            if let Some((key, value)) = self.queue.pop_front() {
                let first = match key {
                    ControlKey::Exposure => std::mem::replace(&mut self.first_exposure_write, false),
                    ControlKey::WhiteBalance => std::mem::replace(&mut self.first_wb_write, false),
                    _ => false,
                };
                let d = if first && self.cfg.slow_first_write {
                    FIRST_WRITE_S
                } else {
                    self.faults.slow.unwrap_or(WRITE_S)
                };
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
                    ControlKey::Exposure => {
                        self.dev_e = l.value;
                        if l.effective {
                            self.eff_e = l.value;
                        }
                    }
                    ControlKey::Gain => {
                        self.dev_g = l.value;
                        if l.effective {
                            self.eff_g = l.value;
                        }
                    }
                    _ => {
                        self.dev_k = l.value;
                        if l.effective {
                            self.eff_k = l.value;
                        }
                    }
                }
            } else {
                i += 1;
            }
        }
        self.max_e_applied = self.max_e_applied.max(self.eff_e);
        let tau = f64::from(self.eff_e).exp2();
        let mut interval = (1.0 / self.cfg.fps).max(if self.cfg.rate_drop {
            tau / self.cfg.rate_drop_fill
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
                    e: self.eff_e,
                    g: self.eff_g,
                    k: self.eff_k,
                    light,
                    cast: self.cast(self.eff_k, (self.temp)(start)),
                },
            ));
        }
    }

    fn step_deliveries(&mut self, ctrl: &mut AutoController) {
        while self.deliveries.front().is_some_and(|(at, _)| *at <= self.t) {
            let (_, c) = self.deliveries.pop_front().expect("checked");
            if self.paused || self.no_samples {
                continue;
            }
            let stats = match (&self.cfg.replay, self.cfg.wb) {
                (Some(r), Some(w)) => r.shot(c.t, c.k, w.alpha_true),
                _ => self.scene.render(c.e, c.g, c.light, c.cast, Some(&mut self.rng)),
            };
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
                Action::ReadBack { key } => {
                    self.readbacks += 1;
                    if self.faults.fail {
                        ctrl.on_read_failed(key, self.t);
                    } else if let Some(info) = self.info(key) {
                        ctrl.on_readback(&info, self.t);
                    }
                }
                Action::HandBack { key, to } => self.handbacks.push((self.t, key, to)),
                Action::Emit => {
                    self.emits += 1;
                    self.emitted.extend(ctrl.emitted().cloned());
                }
            }
        }
        let (o, wb_err) = self.oracle(ctrl.cfg.exposure.target, ctrl.cfg.white_balance.warmth);
        self.history.push(Snap {
            t: self.t,
            err: o.error,
            clip: o.clip,
            e: self.eff_e,
            g: self.eff_g,
            wb_err,
        });
    }
}

/// The controls W4GS reports (the table of 2.2): exposure -13..-1 (default -6), gain 0..100, white balance 2800..6500
/// (default 4600), the rest as the table. Camera auto on exposure, white balance and focus.
pub fn w4gs() -> Vec<ControlInfo> {
    let c = |key, min, max, default, can_auto| ControlInfo {
        key,
        min,
        max,
        step: 1,
        default,
        value: default,
        can_auto,
        can_manual: true,
        auto: false,
        path: AccessPath::Ks,
    };
    vec![
        c(ControlKey::Brightness, -64, 64, 0, false),
        c(ControlKey::Contrast, 0, 64, 32, false),
        c(ControlKey::Hue, -40, 40, 0, false),
        c(ControlKey::Saturation, 0, 128, 64, false),
        c(ControlKey::Sharpness, 0, 6, 3, false),
        c(ControlKey::Gamma, 72, 500, 100, false),
        c(ControlKey::WhiteBalance, 2800, 6500, 4600, true),
        c(ControlKey::BacklightCompensation, 0, 2, 0, false),
        c(ControlKey::Gain, 0, 100, 0, false),
        c(ControlKey::PowerlineFrequency, 0, 2, 1, false),
        c(ControlKey::Exposure, -13, -1, -6, true),
        c(ControlKey::Focus, 1, 1023, 512, true),
    ]
}
