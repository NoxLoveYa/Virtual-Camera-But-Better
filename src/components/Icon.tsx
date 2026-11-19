const PATHS = {
  chevron: "M3.5 6l4.5 4.5L12.5 6",
  check: "M3 8.5l3.2 3.2L13 4.5",
  trash: "M3 4.5h10M6.5 4.5V3h3v1.5M4.5 4.5l.6 8.5h5.8l.6-8.5",
  plus: "M8 3v10M3 8h10",
  up: "M8 13V3.5M4 7l4-3.5L12 7",
  down: "M8 3v9.5M4 9l4 3.5L12 9",
  close: "M4 4l8 8M12 4L4 12",
  undo: "M6 3.5L3 6.5l3 3M3 6.5h6a3.5 3.5 0 010 7H6",
  reset: "M13 8a5 5 0 11-1.5-3.5M12 2v3H9",
  grip: "M6 4v.01M6 8v.01M6 12v.01M10 4v.01M10 8v.01M10 12v.01",
  play: "M5 3.5v9l7.5-4.5z",
  stop: "M4.5 4.5h7v7h-7z",
  pause: "M5.5 3.5v9M10.5 3.5v9",
  more: "M3.5 8v.01M8 8v.01M12.5 8v.01",
  camera: "M2 5.5h2.5L5.7 4h4.6l1.2 1.5H14v7H2zM8 7.1a2.2 2.2 0 100 4.4 2.2 2.2 0 000-4.4z",
  sliders: "M2 4.5h3.5M9 4.5h5M2 8h6M11.5 8H14M2 11.5h2M7.5 11.5H14M7 3v3M10 6.5v3M5 10v3",
  output: "M2 3.5h12v8H2zM6 14h4M8 11.5V14",
  compare: "M8 2.5a5.5 5.5 0 100 11 5.5 5.5 0 000-11zM8 2.5v11M8 5.2a2.8 2.8 0 000 5.6",
  eyedropper: "M10.8 2.4l2.8 2.8-1.6 1.6-2.8-2.8zM9.2 4.4l-5.7 5.7v2.4h2.4l5.7-5.7",
  crop: "M4.5 2v9.5H14M2 4.5h9.5V14",
} as const;

export type IconName = keyof typeof PATHS;

export function Icon({ name, size = 14 }: { name: IconName; size?: number }) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 16 16"
      fill="none"
      stroke="currentColor"
      strokeWidth={name === "grip" || name === "more" ? 2 : 1.5}
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden
    >
      <path d={PATHS[name]} />
    </svg>
  );
}
