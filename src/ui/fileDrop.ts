/**
 * Pure logic of dropping files from Explorer onto the window (W2): what kind of file it is, what a drop does with it
 * where it lands, and what an imported preset is called. No DOM and no React here, so `npm run test:ui` can run it under
 * node. The listener, the highlight and the chooser are in `useFileDrop.ts`.
 *
 * Kinds by extension, case-insensitive. LUT: `.cube`, `.png`. Image: `.png .jpg .jpeg .bmp .gif` (the lists of the file
 * dialog, `file_dialog.rs`). Preset: `.json`. Anything else, folders included, is unsupported.
 */
import { PRESET_NAME } from "../filters/chain";

/** What a path field wants, as `PathField`'s `kind`. */
export type PathKind = "image" | "lut";

export type DropKind = "cube" | "png" | "image" | "preset" | "other";

/** The filters a dropped file can add. */
export type DropFilter = "clut_filter" | "mask_filter";

export const ONE_AT_A_TIME = "Better Cam takes one file at a time.";

/** The extension with its dot, in lower case; "" when the name has none. */
export function extensionOf(path: string): string {
  const name = path.slice(Math.max(path.lastIndexOf("\\"), path.lastIndexOf("/")) + 1);
  const dot = name.lastIndexOf(".");
  return dot > 0 ? name.slice(dot).toLowerCase() : "";
}

/** The file name without its folder and extension. */
export function stemOf(path: string): string {
  const name = path.slice(Math.max(path.lastIndexOf("\\"), path.lastIndexOf("/")) + 1);
  const dot = name.lastIndexOf(".");
  return dot > 0 ? name.slice(0, dot) : name;
}

export function dropKind(path: string): DropKind {
  switch (extensionOf(path)) {
    case ".cube":
      return "cube";
    case ".png":
      return "png";
    case ".jpg":
    case ".jpeg":
    case ".bmp":
    case ".gif":
      return "image";
    case ".json":
      return "preset";
    default:
      return "other";
  }
}

/** Whether a file of this kind is what a path field of `field` kind takes. */
export function fitsField(field: PathKind, kind: DropKind): boolean {
  return field === "lut" ? kind === "cube" || kind === "png" : kind === "png" || kind === "image";
}

/** The first file Better Cam can use, and how many files came with it. */
export function firstSupported(paths: readonly string[]): string | null {
  return paths.find((p) => dropKind(p) !== "other") ?? null;
}

export type DropAction =
  /** The file goes into the path field under the pointer. */
  | { type: "set-path"; path: string }
  /** A new filter with the file as its path, at gap `at` of the chain (`null`: the end). */
  | { type: "add"; filter: DropFilter; path: string; at: number | null }
  /** A `.png` can be a LUT or a mask: ask. */
  | { type: "choose"; path: string; at: number | null }
  | { type: "preset"; path: string }
  | { type: "unsupported"; message: string };

export interface DropRoute {
  action: DropAction;
  /** Added to the toast: only the first of several files was used. */
  note: string | null;
}

export interface DropTarget {
  /** The kind of the path field under the pointer, if one is. */
  field?: PathKind;
  /** The gap of the chain the pointer is in, or `null` when the pointer is not over the chain (a file is appended). */
  gap: number | null;
}

const UNSUPPORTED_TAIL = "Drop a .cube or .png LUT, a picture, or a preset (.json).";

export function unsupportedMessage(path: string): string {
  const ext = extensionOf(path);
  return ext ? `Better Cam can't use ${ext} files. ${UNSUPPORTED_TAIL}` : `Better Cam can't use that file. ${UNSUPPORTED_TAIL}`;
}

/**
 * What dropping `paths` does at `target`. Checked in this order: a path field of a kind the file fits, then the chain
 * (the gap under the pointer), then anywhere else (the end of the chain). Several files: the first supported one wins.
 */
export function routeDrop(paths: readonly string[], target: DropTarget): DropRoute {
  const path = firstSupported(paths);
  if (path === null) return { action: { type: "unsupported", message: unsupportedMessage(paths[0] ?? "") }, note: null };
  const note = paths.length > 1 ? ONE_AT_A_TIME : null;
  const kind = dropKind(path);
  const at = target.gap;
  if (target.field && fitsField(target.field, kind)) return { action: { type: "set-path", path }, note };
  switch (kind) {
    case "cube":
      return { action: { type: "add", filter: "clut_filter", path, at }, note };
    case "image":
      return { action: { type: "add", filter: "mask_filter", path, at }, note };
    case "png":
      return { action: { type: "choose", path, at }, note };
    default:
      return { action: { type: "preset", path }, note };
  }
}

/** The pill shown while a supported file is dragged over the window. */
export function hintFor(path: string, overField: boolean): string {
  if (overField) return "Drop to use this file";
  switch (dropKind(path)) {
    case "cube":
      return "Drop to add Apply LUT";
    case "image":
      return "Drop to add Image Mask/Blend";
    case "png":
      return "Drop to add a LUT or an image mask";
    default:
      return "Drop to import preset";
  }
}

/**
 * The name an imported preset gets: the file name's characters that preset names allow, "Imported" when none are left,
 * and "Name 2", "Name 3"… while a preset of that name exists (compared without regard to case).
 */
export function presetNameFor(stem: string, existing: readonly string[]): string {
  const clean = Array.from(stem)
    .filter((ch) => PRESET_NAME.test(ch))
    .join("")
    .trim()
    .slice(0, 40)
    .trim();
  const base = clean || "Imported";
  const taken = new Set(existing.map((n) => n.toLowerCase()));
  if (!taken.has(base.toLowerCase())) return base;
  for (let n = 2; ; n++) {
    const name = `${base} ${n}`;
    if (!taken.has(name.toLowerCase())) return name;
  }
}

// ---------------------------------------------------------------------------------------------------------------------
// Path fields that take a drop: each registers its root element, so the listener can find the one under the pointer.

export interface FieldTarget {
  kind: PathKind;
  /** Sets the file (the field's `onChange`). */
  drop: (path: string) => void;
  /** Highlights the field while a fitting file is over it. */
  setOver: (over: boolean) => void;
}

const fields = new Map<Element, FieldTarget>();

/** Registers a path field's root element; returns the function that removes it. */
export function registerField(el: Element, target: FieldTarget): () => void {
  fields.set(el, target);
  return () => {
    if (fields.get(el) === target) fields.delete(el);
  };
}

export const fieldAt = (el: Element | null): FieldTarget | null => {
  const root = el?.closest('[data-drop="path"]');
  return (root && fields.get(root)) ?? null;
};
