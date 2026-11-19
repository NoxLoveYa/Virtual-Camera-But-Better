/**
 * Checks of the pure logic behind dropping files from Explorer (W2). `npm run test:ui` bundles and runs it together with
 * the reorder checks. It throws at the first failure and prints `ok` otherwise.
 */
import { presetNameError } from "../filters/chain";
import { ONE_AT_A_TIME, dropKind, extensionOf, fitsField, hintFor, presetNameFor, routeDrop, stemOf, unsupportedMessage, type DropRoute } from "./fileDrop";

function eq<T>(actual: T, expected: T, what: string) {
  if (actual !== expected) throw new Error(`${what}: expected ${String(expected)}, got ${String(actual)}`);
}
const same = (actual: unknown, expected: unknown, what: string) => eq(JSON.stringify(actual), JSON.stringify(expected), what);
const route = (r: DropRoute) => r.action;

// Kinds by extension, case-insensitive.
eq(dropKind("C:\\luts\\warm.cube"), "cube", ".cube");
eq(dropKind("C:\\luts\\WARM.CUBE"), "cube", ".CUBE");
eq(dropKind("a.png"), "png", ".png");
eq(dropKind("a.PNG"), "png", ".PNG");
for (const ext of [".jpg", ".jpeg", ".bmp", ".gif", ".JPG", ".Jpeg"]) eq(dropKind(`a${ext}`), "image", ext);
eq(dropKind("preset.json"), "preset", ".json");
eq(dropKind("notes.txt"), "other", ".txt");
eq(dropKind("C:\\folder.cube\\"), "other", "a folder path ending in a separator");
eq(dropKind("C:\\Users\\me\\Pictures"), "other", "no extension");
eq(dropKind("C:\\Users\\me\\.hidden"), "other", "a leading dot is a name, not an extension");
eq(dropKind("C:\\dir.v2\\file"), "other", "a dot in the folder is not an extension");
eq(dropKind("warm.cube.txt"), "other", "only the last extension counts");
eq(extensionOf("/home/me/x.TAR.Gz"), ".gz", "POSIX separators, lower case");
eq(stemOf("C:\\a b\\Studio light.json"), "Studio light", "stem");
eq(stemOf("noext"), "noext", "stem without extension");

// What a path field takes.
eq(fitsField("lut", "cube"), true, "lut field takes .cube");
eq(fitsField("lut", "png"), true, "lut field takes .png");
eq(fitsField("lut", "image"), false, "lut field refuses a jpg");
eq(fitsField("lut", "preset"), false, "lut field refuses a preset");
eq(fitsField("image", "image"), true, "image field takes a jpg");
eq(fitsField("image", "png"), true, "image field takes .png");
eq(fitsField("image", "cube"), false, "image field refuses .cube");
eq(fitsField("image", "other"), false, "image field refuses other files");

// Routing: a field the file fits comes first.
same(route(routeDrop(["a.cube"], { field: "lut", gap: 1 })), { type: "set-path", path: "a.cube" }, "cube on a lut field");
same(route(routeDrop(["a.jpg"], { field: "image", gap: null })), { type: "set-path", path: "a.jpg" }, "jpg on an image field");
same(route(routeDrop(["a.png"], { field: "image", gap: 0 })), { type: "set-path", path: "a.png" }, "png on an image field");
same(route(routeDrop(["a.png"], { field: "lut", gap: 0 })), { type: "set-path", path: "a.png" }, "png on a lut field");
// A file of the wrong kind falls through to the chain rule.
same(route(routeDrop(["a.jpg"], { field: "lut", gap: 2 })), { type: "add", filter: "mask_filter", path: "a.jpg", at: 2 }, "jpg on a lut field adds a mask");
same(route(routeDrop(["a.cube"], { field: "image", gap: 0 })), { type: "add", filter: "clut_filter", path: "a.cube", at: 0 }, "cube on an image field adds a LUT");
same(route(routeDrop(["p.json"], { field: "image", gap: 0 })), { type: "preset", path: "p.json" }, "preset on a field");

// The chain: the gap is kept. Elsewhere: the end (null).
same(route(routeDrop(["a.cube"], { gap: 1 })), { type: "add", filter: "clut_filter", path: "a.cube", at: 1 }, "cube between two cards");
same(route(routeDrop(["a.CUBE"], { gap: null })), { type: "add", filter: "clut_filter", path: "a.CUBE", at: null }, "cube elsewhere appends");
same(route(routeDrop(["a.bmp"], { gap: 3 })), { type: "add", filter: "mask_filter", path: "a.bmp", at: 3 }, "bmp adds Image Mask");
same(route(routeDrop(["a.gif"], { gap: null })), { type: "add", filter: "mask_filter", path: "a.gif", at: null }, "gif appends Image Mask");
same(route(routeDrop(["a.png"], { gap: 2 })), { type: "choose", path: "a.png", at: 2 }, "png on the chain asks");
same(route(routeDrop(["a.png"], { gap: null })), { type: "choose", path: "a.png", at: null }, "png elsewhere asks");
same(route(routeDrop(["p.json"], { gap: 2 })), { type: "preset", path: "p.json" }, "preset on the chain");
same(route(routeDrop(["p.json"], { gap: null })), { type: "preset", path: "p.json" }, "preset elsewhere");

// Unsupported files and several files.
same(
  route(routeDrop(["C:\\n\\notes.txt"], { gap: 0 })),
  { type: "unsupported", message: "Better Cam can't use .txt files. Drop a .cube or .png LUT, a picture, or a preset (.json)." },
  "txt",
);
eq(unsupportedMessage("C:\\Users\\me\\Pictures"), "Better Cam can't use that file. Drop a .cube or .png LUT, a picture, or a preset (.json).", "folder");
eq(route(routeDrop([], { gap: null })).type, "unsupported", "no files");
eq(routeDrop(["a.cube"], { gap: null }).note, null, "one file: no note");
const many = routeDrop(["x.txt", "b.cube", "c.jpg"], { gap: 1 });
same(many.action, { type: "add", filter: "clut_filter", path: "b.cube", at: 1 }, "the first supported of several");
eq(many.note, ONE_AT_A_TIME, "several files: the note");
eq(routeDrop(["x.txt", "y.txt"], { gap: 1 }).note, null, "nothing usable: no note");

// The hint pill.
eq(hintFor("a.cube", false), "Drop to add Apply LUT", "cube hint");
eq(hintFor("a.jpg", false), "Drop to add Image Mask/Blend", "image hint");
eq(hintFor("a.png", false), "Drop to add a LUT or an image mask", "png hint");
eq(hintFor("p.json", false), "Drop to import preset", "preset hint");
eq(hintFor("a.cube", true), "Drop to use this file", "field hint");

// Preset names.
eq(presetNameFor("Studio light", []), "Studio light", "plain name");
eq(presetNameFor("Studio light", ["studio LIGHT"]), "Studio light 2", "clash is case-insensitive");
eq(presetNameFor("Studio light", ["Studio light", "Studio light 2"]), "Studio light 3", "second clash");
eq(presetNameFor("My (best) preset!", []), "My best preset", "characters that names do not allow are dropped");
eq(presetNameFor("a_b-c 1", []), "a_b-c 1", "allowed characters stay");
eq(presetNameFor("Über Ünïcode", []), "Über Ünïcode", "letters of any script stay");
eq(presetNameFor("!!!", []), "Imported", "nothing left");
eq(presetNameFor("", ["Imported"]), "Imported 2", "nothing left and a clash");
eq(presetNameFor("  padded  ", []), "padded", "trimmed");
eq(presetNameFor("x".repeat(60), []).length, 40, "capped at 40 characters");
eq(presetNameFor("a".repeat(39) + " " + "b".repeat(10), []), "a".repeat(39), "a cut that ends on a space is trimmed");
eq(presetNameFor("😀".repeat(60), []), "Imported", "emoji are not allowed characters");
eq(presetNameFor("é".repeat(60), []), "é".repeat(40), "cut by characters, not code units");
eq(presetNameFor("name...", []), "name", "trailing dots go with the other dropped characters");
eq(presetNameFor("CON", []), "CON preset", "a reserved name gets a suffix");
eq(presetNameFor("nul", []), "nul preset", "reserved without regard to case");
eq(presetNameFor("com1", ["COM1 preset"]), "com1 preset 2", "reserved and a clash");
eq(presetNameFor("LPT9.", []), "LPT9 preset", "reserved once the dot is gone");
eq(presetNameFor("console", []), "console", "a name that only starts with a reserved one is fine");
eq(presetNameFor("COM0", []), "COM0", "COM0 is not reserved");
for (const stem of ["CON", "aux", "x".repeat(200), "a. . .", "  ", "💥", "Prn", "LPT1"]) {
  eq(presetNameError(presetNameFor(stem, [])), null, `an imported name always passes the rule: ${stem}`);
}

// The name rule the menu enforces up front (the backend's `check_preset_name`).
eq(presetNameError(""), null, "empty is nothing to save, not an error");
eq(presetNameError("Studio light"), null, "plain name");
eq(presetNameError("Über_Ünï-1"), null, "allowed characters");
eq(presetNameError("a".repeat(64)), null, "64 characters pass");
eq(presetNameError("é".repeat(64)), null, "64 characters pass, counted as characters");
eq(presetNameError("a".repeat(65)) !== null, true, "65 characters fail");
eq(presetNameError("😀".repeat(2)) !== null, true, "emoji fail");
eq(presetNameError("a.b") !== null, true, "a dot fails");
eq(presetNameError("trailing.") !== null, true, "a trailing dot fails");
eq(presetNameError("trailing ") !== null, true, "a trailing space fails");
eq(presetNameError(" leading"), null, "a leading space is allowed by the backend");
eq(presetNameError("a/b") !== null, true, "a slash fails");
for (const n of ["CON", "con", "Prn", "AUX", "nUl", "COM1", "com9", "LPT1", "lpt9"]) eq(presetNameError(n) !== null, true, `${n} is reserved`);
for (const n of ["COM0", "LPT0", "COM10", "CONSOLE", "CON 1", "my CON"]) eq(presetNameError(n), null, `${n} is not reserved`);

console.log("ok");
