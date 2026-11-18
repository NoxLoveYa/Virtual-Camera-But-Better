/**
 * Checks of the pure logic behind the saved-versus-running format, the pause and the edits made during a virtual camera
 * change (P15 review). There is no browser or test runner in the project: bundle and run it with
 *
 *   npx esbuild src/camera/schema.test.ts --bundle --platform=node --format=esm --outfile=%TEMP%/schema-test.mjs
 *   node %TEMP%/schema-test.mjs
 *
 * It throws at the first failure and prints `ok` otherwise.
 */
import {
  DEFAULT_SOURCE,
  cameraUsers,
  describeSaved,
  inUseText,
  effectiveSource,
  inUseAfterState,
  inUseDeviceDefault,
  inUseFromFallback,
  pausedAfterState,
  savedAfterEdit,
  shouldPushSettings,
  touchesCapture,
  type SourceCfg,
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

console.log("ok");
