/**
 * Checks of the pure logic behind the saved-versus-running format, the pause and the edits made during a virtual camera
 * change (P15 review). There is no browser or test runner in the project: `npm run test:schema` bundles and runs it
 * (esbuild comes with vite), or by hand:
 *
 *   npx esbuild src/camera/schema.test.ts --bundle --platform=node --format=esm --outfile=%TEMP%/schema-test.mjs
 *   node %TEMP%/schema-test.mjs
 *
 * It throws at the first failure and prints `ok` otherwise.
 */
import {
  DEFAULT_AUTO,
  DEFAULT_SOURCE,
  afterVcamRemoval,
  inUseAfterStart,
  cameraUsers,
  describeSaved,
  inUseText,
  effectiveSource,
  inUseAfterState,
  inUseDeviceDefault,
  inUseFromFallback,
  pausedAfterState,
  problemText,
  savedAfterEdit,
  shouldPushSettings,
  stripCodes,
  touchesCapture,
  autoAction,
  autoAdjustWhiteBalance,
  autoCfgOf,
  autoTone,
  cameraIsSlow,
  controlsRecord,
  forgetControls,
  hasModeList,
  isDefaultState,
  limitFraction,
  longestOptions,
  maxGainValue,
  modeOf,
  normalizeAutoCfg,
  normalizeControls,
  normalizeLearned,
  rememberControl,
  requestedFps,
  showsStatus,
  slowCameraText,
  statusOf,
  withAutoCfg,
  withLearned,
  withResume,
  type CameraInfo,
  type ControlInfo,
  type SourceCfg,
  type StoredControls,
} from "./schema";

const same = (got: unknown, want: unknown, what: string) => {
  const a = JSON.stringify(got);
  const b = JSON.stringify(want);
  if (a !== b) throw new Error(`${what}: got ${a}, wanted ${b}`);
};

const saved: SourceCfg = {
  ...DEFAULT_SOURCE,
  video_device_id: "cam",
  res_type: "custom",
  resolution: "1920x1080",
  frame_interval: 166667,
  video_format: "yuy2",
};

// M1: a stop clears the pause, except the one announced for a virtual camera change
same(pausedAfterState(true, { state: "stopped", reason: "vcam-change" }), true, "vcam-change keeps the pause");
same(pausedAfterState(true, { state: "stopped", reason: null }), false, "a stop clears the pause");
same(pausedAfterState(true, { state: "stopped", reason: "error" }), false, "an error clears the pause");
same(pausedAfterState(true, { state: "live" }), true, "other states leave it");
same(pausedAfterState(false, { state: "stopped", reason: "vcam-change" }), false, "and do not set it");

// M2: edits are sent while running and while the virtual camera is changed, never to a stopped engine at rest
same(shouldPushSettings("stopped", null), false, "stopped, nothing under way");
same(shouldPushSettings("stopped", "repair"), true, "during a change the engine is stopped and the edit is parked");
same(shouldPushSettings("standby", null), true, "standby");
same(shouldPushSettings("live", null), true, "live");
same(shouldPushSettings("reconnecting", null), true, "reconnecting");

// M4: the fallback keeps the saved format and records what runs
const fb = inUseFromFallback({ asked: "1920×1080 YUY2 60 fps", width: 1280, height: 720, interval: 333333 });
same(fb.asked, "1920×1080 YUY2 60 fps", "asked");
const running = effectiveSource(saved, fb);
same(
  [running.res_type, running.video_format, running.resolution, running.frame_interval],
  ["preferred", "any", "1280x720", 333333],
  "what runs",
);
same(running.video_device_id, "cam", "the other fields are the saved ones");
same(saved.video_format, "yuy2", "the saved source is not touched");
same(effectiveSource(saved, null), saved, "no fallback: the saved source");

// a flip toggled while the fallback runs is sent with the in-use format, so the engine reopens nothing
const flipped = savedAfterEdit(saved, fb, { flip_vertically: true });
same(flipped.video_format, "yuy2", "an edit of another field keeps the saved format");
same(effectiveSource(flipped, fb).resolution, "1280x720", "and the engine is sent the in-use format");
same(touchesCapture({ flip_vertically: true }), false, "flip is not a capture field");
same(touchesCapture({ color_space: "709" }), false, "colour is not a capture field");
for (const k of ["video_device_id", "res_type", "resolution", "frame_interval", "video_format", "hw_decode", "buffering"] as const) {
  same(touchesCapture({ [k]: saved[k] }), true, `${k} is a capture field`);
}
// an edit of a camera field while it runs starts from what the fields show, and replaces the saved choice
const edited = savedAfterEdit(saved, fb, { frame_interval: 333333, res_type: "custom" });
same([edited.res_type, edited.video_format, edited.resolution, edited.frame_interval], ["custom", "any", "1280x720", 333333], "edit");
same(savedAfterEdit(saved, null, { resolution: "1280x720" }).video_format, "yuy2", "without a fallback it is the plain merge");

// a stop clears the fallback (the next Start tries the saved format), the one for a virtual camera change does not
same(inUseAfterState(fb, { state: "stopped", reason: null }), null, "stop clears the fallback");
same(inUseAfterState(fb, { state: "stopped", reason: "error" }), null, "error clears the fallback");
same(inUseAfterState(fb, { state: "stopped", reason: "vcam-change" }), fb, "vcam-change keeps the fallback");
same(inUseAfterState(fb, { state: "live" }), fb, "live keeps the fallback");

// a removal of the virtual camera that did not happen (declined, failed) leaves the running state as it is: the backend
// brings the engine back with it. One that happened ends the pause, the fallback and the start at the next launch.
const state = { paused: true, inUse: fb, wasRunning: true };
same(afterVcamRemoval(false, state), state, "a removal that did not happen keeps the state");
same(afterVcamRemoval(true, state), { paused: false, inUse: null, wasRunning: false }, "a removal that happened clears it");
same(afterVcamRemoval(false, { paused: false, inUse: null, wasRunning: false }), { paused: false, inUse: null, wasRunning: false }, "nothing to keep");

// a Start the backend rejected leaves no "using the device default" panel behind
const wanted = inUseDeviceDefault(saved);
same(inUseAfterStart(wanted, true), wanted, "an accepted Start keeps the running format");
same(inUseAfterStart(wanted, false), null, "a rejected Start runs nothing");
same(inUseAfterStart(null, false), null, "a normal Start has none");

// L5: the device default as the running format keeps the saved size and rate, which is what the engine is sent
const dflt = inUseDeviceDefault(saved);
same(dflt.asked, "1920×1080 YUY2 60 fps", "asked");
const sent = effectiveSource(saved, dflt);
same([sent.res_type, sent.video_format, sent.resolution, sent.frame_interval], ["preferred", "any", "1920x1080", 166667], "sent");
same(saved.res_type, "custom", "the saved source stays custom");

// the sentence for the saved format, as the backend words it
same(describeSaved(saved), "1920×1080 YUY2 60 fps", "describe");
same(describeSaved({ ...saved, frame_interval: 0 }), "1920×1080 YUY2", "no rate");
same(describeSaved({ ...saved, frame_interval: 333667, video_format: "mjpeg" }), "1920×1080 MJPEG 29.97 fps", "29.97");
same(describeSaved({ ...saved, res_type: "preferred", video_format: "nv12" }), "NV12", "device default size");
same(describeSaved(DEFAULT_SOURCE), "device default", "device default");

// who holds the camera: none, one, two, more (the backend's `reason` says the same in lower case)
same(cameraUsers(null), "another app", "no list");
same(cameraUsers([]), "another app", "empty list");
same(cameraUsers(["Discord.exe"]), "Discord.exe", "one");
same(cameraUsers(["Discord.exe", "chrome.exe"]), "Discord.exe and chrome.exe", "two");
same(cameraUsers(["a.exe", "b.exe", "c.exe"]), "a.exe, b.exe and 1 more", "three");
same(cameraUsers(["Discord.exe", "chrome.exe", "c.exe", "d.exe"]), "Discord.exe, chrome.exe and 2 more", "four");
same(inUseText(["Discord.exe"]), "The camera is in use by Discord.exe.", "sentence");
same(inUseText(undefined), "The camera is in use by another app.", "sentence without a list");
same(inUseText(["Discord.exe"], true), "The camera may be in use by Discord.exe.", "may be");
same(inUseText([], true), "The camera is in use by another app.", "nobody named, nothing to doubt");

// P10: visible text never shows a hex code; the engine's own words stay available for a tooltip
same(stripCodes("Camera disconnected (0xC00D3EA2)"), "Camera disconnected", "one code");
same(stripCodes("failed (0xC00D3E8C) then (0x8007001F)"), "failed then", "two codes");
same(stripCodes("path C:\\x (0x12) stays"), "path C:\\x (0x12) stays", "not an 8-digit code: left alone");
same(stripCodes("Stream error(0xc00d3e8c)"), "Stream error", "lower case, no space");
same(stripCodes("no code"), "no code", "nothing to strip");

const raw = "camera disconnected (0xC00D3EA2)";
same(problemText("disconnected", raw), {
  short: "Camera disconnected",
  long: "The camera was disconnected. Plug it in again; Better Cam goes live as soon as it is back.",
}, "disconnected");
same(problemText("not_found", null), {
  short: "Camera not found",
  long: "Camera not found. Connect it or pick another camera; Better Cam goes live as soon as it is there.",
}, "not found");
same(problemText("in_use", "camera is in use by Discord.exe", ["Discord.exe"], false), {
  short: "The camera is in use by Discord.exe.",
  long: "The camera is in use by Discord.exe. Better Cam goes live as soon as it is free.",
}, "in use");
same(problemText("in_use", null, [], false).short, "The camera is in use by another app.", "in use, nobody named");
same(problemText("in_use", null, ["Zoom.exe"], true).short, "The camera may be in use by Zoom.exe.", "in use, maybe");
same(problemText("privacy", "privacy: desktop apps").short, "Camera blocked by Windows privacy settings", "privacy short");
same(problemText("privacy", "blocked by a policy").long.startsWith("Windows is not letting apps use the camera: a Windows policy"), true, "policy");
same(problemText("privacy", "desktop apps are off").long.includes("“Let desktop apps access your camera”"), true, "desktop apps");
same(problemText("privacy", "off").long.includes("“Camera access”"), true, "camera access");
same(problemText("error", "the camera stream failed (0xC00D3E8C)"), {
  short: "The camera stream failed",
  long: "The camera stream failed. Better Cam keeps trying for a little while.",
}, "generic: code stripped");
same(problemText("error", null), {
  short: "The camera stopped sending pictures",
  long: "The camera is not answering. Better Cam keeps trying for a little while.",
}, "generic without a reason");
same(problemText(null, "Waiting.", [], false).long, "Waiting. Better Cam keeps trying for a little while.", "a reason that ends in a full stop");
for (const kind of ["in_use", "disconnected", "not_found", "privacy", "error", null]) {
  const p = problemText(kind, raw, ["a.exe"], false);
  same(/0x[0-9A-Fa-f]{8}/.test(p.short + p.long), false, `no code in the text for ${kind}`);
}

// ---------------------------------------------------------------------------------------------------------------
// Better Cam auto (docs/AUTO-CONTROLS.md 5.1, 6.2): the stored record

const cam = { id: "\\\\?\\usb#vid_0c45&pid_636d#1&2&3#{guid}\\global", name: "W4GS" };
const exposureInfo: ControlInfo = {
  key: "exposure", min: -13, max: -1, step: 1, def: -6, value: -5, auto: false, can_auto: true, can_manual: true, mode: "app", app: "full",
};
const gainInfo: ControlInfo = { key: "gain", min: 0, max: 100, step: 1, def: 0, value: 48, auto: false, can_auto: false, can_manual: true, mode: "app", app: null, driven_by: "exposure" };

// old saves load unchanged: no mode, no auto, no learned
const old = { [cam.id]: { name: "W4GS", values: { brightness: { value: 20, auto: false }, focus: { value: 300, auto: true } } } };
same(normalizeControls(JSON.parse(JSON.stringify(old))), old, "an old save loads as it is");
same(normalizeControls(null), {}, "nothing saved");
same(normalizeControls({ [cam.id]: { name: "x", values: {} } }), {}, "an empty record is not kept");

// valid mode / auto / learned stay, invalid ones go
const newSave = {
  [cam.id]: {
    name: "W4GS",
    values: {
      exposure: { value: -5, auto: false, mode: "app" },
      gain: { value: 48, auto: false, mode: "app" },
      white_balance: { value: 4650, auto: false, mode: "app" },
      brightness: { value: 3, auto: false, mode: "app" },
      focus: { value: 300, auto: true, mode: "banana" },
    },
    auto: { exposure: { target: 0.5, longest: -6, max_gain: 60, speed: "fast" }, white_balance: { warmth: -0.3, speed: "slow" } },
    learned: { gain_stops: 2.5, wb_alpha: 0.006, rate_caps: { "30": -6 }, blocked: ["white_balance"] },
  },
};
const read = normalizeControls(JSON.parse(JSON.stringify(newSave)));
same(read[cam.id].values.exposure, { value: -5, auto: false, mode: "app" }, "exposure app kept");
same(read[cam.id].values.gain, { value: 48, auto: false, mode: "app" }, "gain app kept with the exposure");
same(read[cam.id].values.white_balance, { value: 4650, auto: false, mode: "app" }, "white balance app kept");
same(read[cam.id].values.brightness, { value: 3, auto: false }, "app on a control Better Cam does not drive: dropped, the value stays");
same(read[cam.id].values.focus, { value: 300, auto: true }, "an unknown mode: dropped, the Auto flag stays");
same(read[cam.id].auto, newSave[cam.id].auto, "auto kept");
same(read[cam.id].learned, newSave[cam.id].learned, "learned kept");
same(
  normalizeControls({ [cam.id]: { name: "", values: { gain: { value: 1, auto: false, mode: "app" } } } })[cam.id].values.gain,
  { value: 1, auto: false },
  "gain app without exposure app: Manual",
);
same(
  normalizeControls({ [cam.id]: { name: "", values: { exposure: { value: -5, auto: true, mode: "app" } } } })[cam.id].values.exposure,
  { value: -5, auto: true },
  "app with the Auto flag on is a contradiction: Camera auto",
);
same(
  normalizeAutoCfg({ exposure: { target: "dark", longest: "never", max_gain: -4, speed: "warp" }, white_balance: { warmth: "hot" } }),
  DEFAULT_AUTO,
  "bad auto fields are the defaults",
);
same(normalizeAutoCfg({ exposure: { target: 7 } })!.exposure.target, 0.8, "target clamped down");
same(normalizeAutoCfg({ exposure: { target: 0.01 }, white_balance: { warmth: 9 } })!.exposure.target, 0.2, "target clamped up");
same(normalizeAutoCfg({ white_balance: { warmth: 9 } })!.white_balance.warmth, 1, "warmth clamped");
same(normalizeAutoCfg({ exposure: { longest: 99 } })!.exposure.longest, "fit", "longest out of range: fit");
same(normalizeAutoCfg("x"), undefined, "auto that is no object");
same(DEFAULT_AUTO.white_balance.warmth, 0, "the default warmth stays neutral (AUTO-WHITE-BALANCE.md D1)");
same(normalizeAutoCfg({ white_balance: { warmth: 0.25 } })!.white_balance.warmth, 0.25, "a stored warmth is kept");

// The Auto Adjust that finishes the colour past the camera's warm end (AUTO-WHITE-BALANCE.md 4)
const adjustOn = { kind: "auto_adjust", enabled: true, params: { exposure: true, white_balance: true, temperature: 0.2 } };
same(autoAdjustWhiteBalance([{ kind: "blur", enabled: true, params: {} }, adjustOn]), adjustOn, "Auto Adjust with white balance on is found");
same(autoAdjustWhiteBalance([]), undefined, "no Auto Adjust");
same(autoAdjustWhiteBalance([{ ...adjustOn, enabled: false }]), undefined, "a disabled Auto Adjust does not finish anything");
same(autoAdjustWhiteBalance([{ ...adjustOn, params: { white_balance: false } }]), undefined, "Auto Adjust with white balance off does not either");
same(
  normalizeLearned({ gain_stops: 99, wb_alpha: 0, rate_caps: { "30": -99, x: 1, "60": "no" }, blocked: ["exposure", "gain", "exposure"], extra: 1 }),
  { gain_stops: 6, wb_alpha: 0.002, rate_caps: { "30": -16 }, blocked: ["exposure"] },
  "learned clamped, unknown dropped",
);
same(normalizeLearned({ gain_stops: "x", blocked: [] }), undefined, "learned with nothing valid");
same(normalizeControls({ [cam.id]: { name: "W4GS", values: {}, learned: { gain_stops: 3 } } })[cam.id].learned, { gain_stops: 3 }, "a record with only learned values is kept");

// rememberControl stores app controls, with the value as the resume point
same(isDefaultState({ value: -6, def: -6, auto: false, can_auto: false, mode: "app" }), false, "app is never the default state");
let stored: StoredControls = {};
stored = rememberControl(stored, cam, exposureInfo);
stored = rememberControl(stored, cam, gainInfo);
same(stored[cam.id].values, { exposure: { value: -5, auto: false, mode: "app" }, gain: { value: 48, auto: false, mode: "app" } }, "app controls stored");
// back to the device's own state (Camera auto): the entry goes
same(
  rememberControl(stored, cam, { ...exposureInfo, mode: "camera", auto: true })[cam.id].values,
  { gain: { value: 48, auto: false, mode: "app" } },
  "Camera auto is the default state again",
);
// Manual keeps no mode
same(rememberControl(stored, cam, { ...exposureInfo, mode: "manual", value: -7 })[cam.id].values.exposure, { value: -7, auto: false }, "Manual: no mode");

// settings, learned values and resume points
stored = withAutoCfg(stored, cam, { ...DEFAULT_AUTO, exposure: { ...DEFAULT_AUTO.exposure, target: 0.6 } });
same(autoCfgOf(stored, cam).exposure.target, 0.6, "auto settings stored");
same(autoCfgOf({}, cam), DEFAULT_AUTO, "defaults without a record");
same(autoCfgOf(withAutoCfg(stored, cam, DEFAULT_AUTO), cam), DEFAULT_AUTO, "default settings are not kept");
same(withAutoCfg({}, cam, DEFAULT_AUTO), {}, "nothing to keep, no record");
stored = withLearned(stored, cam, { gain_stops: 2.2 });
same(stored[cam.id].learned, { gain_stops: 2.2 }, "learned stored");
const resumed = withResume(stored, cam, { exposure: -6, gain: 60, white_balance: 5000 });
same(resumed[cam.id].values.exposure!.value, -6, "resume point of exposure");
same(resumed[cam.id].values.gain!.value, 60, "resume point of gain");
same(resumed[cam.id].values.white_balance, undefined, "no resume for a control that is not in Better Cam auto");
same(withResume(stored, cam, { exposure: -5 }), stored, "an unchanged resume point changes nothing");
same(withResume({}, cam, { exposure: -5 }), {}, "no record, no resume");

// Reset all keeps the settings and what was learned
const forgotten = forgetControls(stored, cam);
same(forgotten[cam.id].values, {}, "Reset all drops the controls");
same(forgotten[cam.id].auto, stored[cam.id].auto, "Reset all keeps auto");
same(forgotten[cam.id].learned, stored[cam.id].learned, "Reset all keeps learned");
same(forgetControls(withResume(rememberControl({}, cam, { ...exposureInfo, mode: "manual", value: -7 }), cam, {}), cam), {}, "Reset all with nothing else leaves no record");
same(forgetControls({}, cam), {}, "Reset all without a record");

// controlsRecord sends all of it
const rec = controlsRecord(stored, cam)!;
same(rec.controls.exposure, { value: -5, auto: false, mode: "app" }, "record: mode");
same(rec.auto, stored[cam.id].auto, "record: auto");
same(rec.learned, { gain_stops: 2.2 }, "record: learned");
same(controlsRecord(old as unknown as StoredControls, cam)!, { id: cam.id, name: "W4GS", controls: old[cam.id].values }, "record of an old save has no extra fields");
same(controlsRecord({}, cam), { id: cam.id, name: "W4GS", controls: {} }, "record without anything stored");
same(controlsRecord({}, null), null, "no camera, no record");
// a camera that moved to another port keeps its settings
const moved = { id: "\\\\?\\usb#vid_0c45&pid_636d#9&9&9#{guid}\\global", name: "W4GS" };
same(autoCfgOf(stored, moved).exposure.target, 0.6, "settings follow the camera to another port");

// modes on the wire and the card
same(modeOf({ auto: true }), "camera", "no mode from an older backend: the Auto flag");
same(modeOf({ auto: false }), "manual", "no mode from an older backend: Manual");
same(modeOf(exposureInfo), "app", "mode from the backend");
same(hasModeList(exposureInfo), true, "exposure has the mode list");
same(hasModeList(gainInfo), false, "gain has none");
same(hasModeList({ key: "focus", mode: "manual", app: null }), false, "focus keeps the Auto switch");
same(hasModeList({ key: "white_balance", mode: "manual", app: null, app_why: "This camera has no manual white balance." }), true, "not offered: still listed");

// the status line
const stat = { id: cam.id, exposure: { state: "holding", message: "Holding 30 fps · gain 48 of 75" }, white_balance: { state: "ignored", message: "x", handed_back: "camera" } };
same(statusOf(stat as never, cam, "exposure")?.state, "holding", "status of this camera");
same(statusOf(stat as never, moved, "exposure")?.state, "holding", "status follows the port");
same(statusOf(stat as never, { id: "\\\\?\\usb#vid_1111&pid_2222#1#{g}\\global" }, "exposure"), null, "status of another camera");
same(statusOf(null, cam, "exposure"), null, "no status");
same(showsStatus("app", { state: "holding" }), true, "shown in Better Cam auto");
same(showsStatus("manual", { state: "holding" }), false, "a stale line is not shown in Manual");
same(showsStatus("camera", { state: "ignored", handed_back: "camera" }), true, "after a hand-back to Camera auto: Try again");
same(showsStatus("manual", { state: "ignored", handed_back: "camera" }), false, "not once the control moved on");
same(showsStatus("camera", null), false, "no status");
same(autoAction("conflict"), "resume", "Resume after a conflict");
same(autoAction("ignored"), "retry", "Try again after ignored");
same(autoAction("holding"), null, "no button while it works");
same(autoTone("holding"), "neutral", "tone: working");
same(autoTone("limit_gain"), "warn", "tone: limit");
same(autoTone("hunting"), "error", "tone: stopped");
same(autoTone("slow"), "neutral", "tone: slow lane (it is working)");
same(autoAction("slow"), null, "no button on the slow lane");

// limits on the sliders and the "Longest exposure" list
const stops = longestOptions({ min: -13, max: -1 }, 30);
same(stops[0], { value: "fit", label: "Fit the frame rate (1/32 s at 30 fps, one stop longer in a dark room at most gain)" }, "fit option");
same(longestOptions({ min: -13, max: -5 }, 30)[0].label, "Fit the frame rate (1/32 s at 30 fps)", "fit option: no longer stop in the range, no hint");
same(stops.length, 14, "fit and 13 whole stops");
same(stops.find((o) => o.value === "-6")?.label, "1/64 s (sharper motion, more gain)", "shorter than the frame time");
same(stops.find((o) => o.value === "-5")?.label, "1/32 s (the frame-rate limit)", "at the frame time");
same(stops.find((o) => o.value === "-4")?.label, "1/16 s (camera drops to ≈ 16 fps)", "longer than the frame time: min(F, 1/τ), 16.2 fps measured");
same(stops.find((o) => o.value === "-3")?.label, "1/8 s (camera drops to ≈ 8 fps)", "1/8 s");
same(stops.find((o) => o.value === "-1")?.label, "1/2 s (camera drops to ≈ 2 fps)", "1/2 s");
// A rate below what the frame time alone gives is never above the asked rate (F = 15: the limit is 1/16 s, 1/8 s is 8).
same(longestOptions({ min: -6, max: -1 }, 15).find((o) => o.value === "-3")?.label, "1/8 s (camera drops to ≈ 8 fps)", "slow camera");
same(longestOptions({ min: -13, max: -1 }, 60)[0].label, "Fit the frame rate (1/64 s at 60 fps, one stop longer in a dark room at most gain)", "fit at 60 fps");
same(longestOptions({ min: -3, max: -1 }, null).map((o) => o.label), ["Fit the frame rate", "1/8 s", "1/4 s", "1/2 s"], "no frame rate: no meanings");
same(maxGainValue(null, { min: 0, max: 100, step: 1 }), 75, "default most gain: 75 % of the range");
same(maxGainValue(null, { min: 0, max: 100, step: 10 }), 80, "default most gain on the step grid");
same(maxGainValue(500, { min: 0, max: 100, step: 1 }), 100, "most gain clamped");
same(limitFraction(-5, { min: -13, max: -1 }), 8 / 12, "limit tick position");
same(limitFraction(5, { min: -13, max: -1 }), null, "a limit outside the range has no tick");
same(limitFraction(null, { min: 0, max: 1 }), null, "no limit");

// the slow-camera hint (F6): the rate asked for, and when Camera auto is slowing the camera
const w4gs: CameraInfo = {
  id: "cam",
  name: "W4GS",
  formats: [
    { format: "mjpeg", width: 1280, height: 720, intervals: [166667, 333333] },
    { format: "yuy2", width: 1280, height: 720, intervals: [333333] },
  ],
};
const open720 = { width: 1280, height: 720, format: "mjpeg" };
same(requestedFps({ res_type: "custom", frame_interval: 166667 }, { fps: "source" }, w4gs, open720), 1e7 / 166667, "custom rate");
same(requestedFps({ res_type: "custom", frame_interval: 0 }, { fps: "source" }, w4gs, open720), 1e7 / 166667, "highest rate of the open format");
same(requestedFps({ res_type: "custom", frame_interval: -1 }, { fps: 30 }, w4gs, open720), 1e7 / 333333, "nearest rate to the output's");
same(requestedFps({ res_type: "custom", frame_interval: -1 }, { fps: "source" }, w4gs, open720), 1e7 / 166667, "same as camera: highest");
same(requestedFps({ res_type: "custom", frame_interval: 0 }, { fps: "source" }, w4gs, { ...open720, format: "yuy2" }), 1e7 / 333333, "rates of that format only");
same(requestedFps({ res_type: "custom", frame_interval: 0 }, { fps: "source" }, w4gs, null), null, "nothing open");
same(requestedFps({ res_type: "preferred", frame_interval: 166667 }, { fps: "source" }, w4gs, open720), null, "device default: unknown");
// the open media type's own rate (stats.capture.rate) answers for every setting, Device default included
same(requestedFps({ res_type: "preferred", frame_interval: 0 }, { fps: "source" }, w4gs, { ...open720, rate: 30 }), 30, "device default: the open type's rate");
same(requestedFps({ res_type: "preferred", frame_interval: 0 }, { fps: "source" }, null, { ...open720, rate: 60 }), 60, "device default, camera list not loaded");
same(requestedFps({ res_type: "custom", frame_interval: 166667 }, { fps: "source" }, w4gs, { ...open720, rate: 30 }), 30, "what opened wins over what was saved");
same(requestedFps({ res_type: "preferred", frame_interval: 0 }, { fps: "source" }, w4gs, { ...open720, rate: 0 }), null, "no rate open: unknown");
same(cameraIsSlow(14.3, 60), true, "14 of 60");
same(cameraIsSlow(55, 60), false, "92 % is not slow");
same(cameraIsSlow(53.9, 60), true, "just under 90 %");
same(cameraIsSlow(0, 60), false, "no frames is not a slow camera");
same(cameraIsSlow(14, null), false, "no rate asked for");
same(slowCameraText(14.3, 60), "Camera auto is slowing the camera to 14 fps in this light. Better Cam auto keeps 60 fps.", "hint text");

console.log("ok");
