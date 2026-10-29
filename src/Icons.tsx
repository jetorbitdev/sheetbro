import type { ReactNode, SVGProps } from "react";

// one family: 16px grid, 1.3 stroke, round caps — drawn in currentColor
const Svg = ({ children, ...rest }: SVGProps<SVGSVGElement> & { children: ReactNode }) => (
  <svg
    width="16"
    height="16"
    viewBox="0 0 16 16"
    fill="none"
    stroke="currentColor"
    strokeWidth="1.3"
    strokeLinecap="round"
    strokeLinejoin="round"
    aria-hidden="true"
    {...rest}
  >
    {children}
  </svg>
);

export const UndoIcon = () => (
  <Svg>
    <path d="M6 3.5 3 6.5l3 3" />
    <path d="M3.5 6.5h5.25a3.25 3.25 0 0 1 0 6.5H6.5" />
  </Svg>
);

export const RedoIcon = () => (
  <Svg>
    <path d="m10 3.5 3 3-3 3" />
    <path d="M12.5 6.5H7.25a3.25 3.25 0 0 0 0 6.5H9.5" />
  </Svg>
);

export const AlignLeftIcon = () => (
  <Svg>
    <path d="M2.5 3.5h11M2.5 6.5h7M2.5 9.5h11M2.5 12.5h7" />
  </Svg>
);

export const AlignCenterIcon = () => (
  <Svg>
    <path d="M2.5 3.5h11M4.5 6.5h7M2.5 9.5h11M4.5 12.5h7" />
  </Svg>
);

export const AlignRightIcon = () => (
  <Svg>
    <path d="M2.5 3.5h11M6.5 6.5h7M2.5 9.5h11M6.5 12.5h7" />
  </Svg>
);

export const MergeIcon = () => (
  <Svg>
    <rect x="1.5" y="3.5" width="13" height="9" rx="1.5" />
    <path d="M4.5 8h7M6.5 6l-2 2 2 2M9.5 6l2 2-2 2" />
  </Svg>
);

// paint bucket + a bar showing the colour that will be applied
export const FillColorIcon = ({ color }: { color: string }) => (
  <Svg>
    <path d="M7.5 1.8 12.4 6.7a1.1 1.1 0 0 1 0 1.5L8.9 11.7a1.1 1.1 0 0 1-1.5 0L3.3 7.6a1.1 1.1 0 0 1 0-1.5L7.5 1.8Z" />
    <path d="M3.4 6.9h8.9" />
    <path d="M13.3 8.9c.7.9 1 1.4 1 1.8a1 1 0 0 1-2 0c0-.4.3-.9 1-1.8Z" />
    <rect x="2" y="13" width="12" height="2" rx="1" fill={color} stroke="#d9cf9f" strokeWidth="0.6" />
  </Svg>
);

export const TextColorIcon = ({ color }: { color: string }) => (
  <Svg>
    <path d="M4.2 10.8 8 2.6l3.8 8.2M5.6 8h4.8" />
    <rect x="2" y="13" width="12" height="2" rx="1" fill={color} stroke="none" />
  </Svg>
);

export const NoFillIcon = () => (
  <Svg>
    <rect x="2.5" y="2.5" width="11" height="11" rx="2.5" />
    <path d="m3.8 12.2 8.4-8.4" />
  </Svg>
);

export const PlusIcon = () => (
  <Svg>
    <path d="M8 3.5v9M3.5 8h9" />
  </Svg>
);

export const CutIcon = () => (
  <Svg>
    <circle cx="4.5" cy="11.8" r="1.9" />
    <circle cx="11.5" cy="11.8" r="1.9" />
    <path d="M5.5 10.2 11 2.5M10.5 10.2 5 2.5" />
  </Svg>
);

export const CopyIcon = () => (
  <Svg>
    <rect x="5.5" y="5.5" width="8" height="8" rx="1.6" />
    <path d="M10.5 5.5V4a1.5 1.5 0 0 0-1.5-1.5H4A1.5 1.5 0 0 0 2.5 4v5A1.5 1.5 0 0 0 4 10.5h1.5" />
  </Svg>
);

export const PasteIcon = () => (
  <Svg>
    <rect x="3" y="3" width="10" height="10.5" rx="1.6" />
    <path d="M6 3v-.5a.5.5 0 0 1 .5-.5h3a.5.5 0 0 1 .5.5V3M5.5 7.5h5M5.5 10.2h3" />
  </Svg>
);

export const TrashIcon = () => (
  <Svg>
    <path d="M3 4.5h10M6.5 4.5V3h3v1.5M4.5 4.5l.6 8.2a1 1 0 0 0 1 .8h3.8a1 1 0 0 0 1-.8l.6-8.2M7 7v3.5M9 7v3.5" />
  </Svg>
);

export const ClearIcon = () => (
  <Svg>
    <path d="m4.5 4.5 7 7M11.5 4.5l-7 7" />
  </Svg>
);

// four-point sparkle: the "AI" star, sitting in the top-right corner of the 16px grid
const SPARKLE = "M12.6.9l.72 1.98 1.98.72-1.98.72-.72 1.98-.72-1.98-1.98-.72 1.98-.72z";

export const StarIcon = () => (
  <Svg>
    <path d="M8 1.5l1.7 4.8 4.8 1.7-4.8 1.7L8 14.5l-1.7-4.8L1.5 8l4.8-1.7Z" fill="currentColor" />
  </Svg>
);

// two linked rings (a connection) plus the star: an AI tool plugged into the sheet
export const McpIcon = () => (
  <Svg>
    {/* the rings are drawn smaller than the grid so the star has its own corner; the wider stroke keeps the weight */}
    <g transform="translate(.2 2.4) scale(.88)" strokeWidth="1.48">
      <path d="M6.6 9.4a2.7 2.7 0 0 0 4.1.3l1.6-1.6a2.7 2.7 0 0 0-3.8-3.8l-.9.9" />
      <path d="M9.4 6.6a2.7 2.7 0 0 0-4.1-.3L3.7 7.9a2.7 2.7 0 0 0 3.8 3.8l.9-.9" />
    </g>
    <path d={SPARKLE} fill="currentColor" stroke="none" />
  </Svg>
);

// the app icon in miniature, for the title bar
export const AppMark = () => (
  <svg width="28" height="28" viewBox="0 0 28 28" aria-hidden="true">
    <defs>
      <linearGradient id="appmark-bg" x1="0" y1="0" x2="0" y2="1">
        <stop offset="0" stopColor="#2f7d5c" />
        <stop offset="1" stopColor="#1a4d38" />
      </linearGradient>
    </defs>
    <rect width="28" height="28" rx="7" fill="url(#appmark-bg)" />
    <rect x="5" y="5" width="18" height="18" rx="3" fill="#fbfaf6" />
    <path d="M5 10V8a3 3 0 0 1 3-3h12a3 3 0 0 1 3 3v2Z" fill="#cfe4d8" />
    <path d="M11 10v13M17 10v13M5 14.5h18M5 18.8h18" stroke="#ddd9ca" strokeWidth="0.8" />
    <rect x="11" y="14.5" width="6" height="4.3" fill="#1f5a43" fillOpacity="0.16" stroke="#1f5a43" strokeWidth="1.3" rx="0.4" />
    <rect x="16.1" y="17.9" width="2.1" height="2.1" rx="0.5" fill="#1f5a43" stroke="#fbfaf6" strokeWidth="0.7" />
  </svg>
);
