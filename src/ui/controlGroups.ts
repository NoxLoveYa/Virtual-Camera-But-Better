/**
 * How the inspector groups the camera's controls for display (docs/LAYOUT-INSPECTOR.md 2.3). Display only: the store keeps
 * its own order (`CONTROL_KEYS`). Pure: no DOM, no store.
 */
import { CONTROL_KEYS, type ControlKey } from "../camera/schema";

export const CONTROL_GROUPS: readonly { title: string; keys: readonly ControlKey[] }[] = [
  { title: "Exposure and colour", keys: ["exposure", "gain", "iris", "white_balance", "backlight_compensation", "low_light_compensation", "powerline_frequency"] },
  { title: "Picture", keys: ["brightness", "contrast", "gamma", "saturation", "hue", "sharpness", "color_enable"] },
  { title: "Lens", keys: ["focus", "zoom", "pan", "tilt", "roll"] },
];

/**
 * The reported controls in their groups, each group in the order above. A group none of whose controls the camera reports is
 * left out; a control no group names (a key added to `CONTROL_KEYS` later) goes at the end of the last group, so none is lost.
 */
export function groupControls<T extends { key: ControlKey }>(infos: readonly T[]): { title: string; items: T[] }[] {
  const named = new Set<ControlKey>(CONTROL_GROUPS.flatMap((g) => g.keys));
  const rest = CONTROL_KEYS.filter((k) => !named.has(k));
  return CONTROL_GROUPS.map((g, i) => {
    const keys = i === CONTROL_GROUPS.length - 1 ? [...g.keys, ...rest] : g.keys;
    return { title: g.title, items: keys.flatMap((k) => infos.filter((info) => info.key === k)) };
  }).filter((g) => g.items.length > 0);
}
