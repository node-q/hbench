// One hand-drawn 16px stroke icon set (1.5px strokes, square caps). Add new glyphs here, same grid.
import type { SVGProps } from "react";

type P = SVGProps<SVGSVGElement> & { size?: number };

function I({ size = 16, children, ...rest }: P & { children: React.ReactNode }) {
  return (
    <svg width={size} height={size} viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth={1.5} strokeLinecap="square" aria-hidden="true" {...rest}>
      {children}
    </svg>
  );
}

export const Search = (p: P) => (
  <I {...p}>
    <circle cx="7" cy="7" r="4.25" />
    <path d="M10.5 10.5 13.5 13.5" />
  </I>
);
export const Chevron = (p: P) => (
  <I {...p}>
    <path d="M6 4l4 4-4 4" />
  </I>
);
export const ChevronDown = (p: P) => (
  <I {...p}>
    <path d="M4 6l4 4 4-4" />
  </I>
);
export const ArrowUp = (p: P) => (
  <I {...p}>
    <path d="M8 13V3M4 7l4-4 4 4" />
  </I>
);
export const ArrowDown = (p: P) => (
  <I {...p}>
    <path d="M8 3v10M4 9l4 4 4-4" />
  </I>
);
export const ArrowLeft = (p: P) => (
  <I {...p}>
    <path d="M13 8H3M7 4 3 8l4 4" />
  </I>
);
export const ArrowRight = (p: P) => (
  <I {...p}>
    <path d="M3 8h10M9 4l4 4-4 4" />
  </I>
);
export const Close = (p: P) => (
  <I {...p}>
    <path d="M4 4l8 8M12 4l-8 8" />
  </I>
);
export const Check = (p: P) => (
  <I {...p}>
    <path d="M3 8.5 6.5 12 13 4.5" />
  </I>
);
export const Copy = (p: P) => (
  <I {...p}>
    <rect x="5.5" y="5.5" width="8" height="8" />
    <path d="M10.5 5.5v-3h-8v8h3" />
  </I>
);
export const Filter = (p: P) => (
  <I {...p}>
    <path d="M2.5 3.5h11M4.5 8h7M6.5 12.5h3" />
  </I>
);
export const Columns = (p: P) => (
  <I {...p}>
    <rect x="2.5" y="3" width="11" height="10" />
    <path d="M6.2 3v10M9.8 3v10" />
  </I>
);
export const Trash = (p: P) => (
  <I {...p}>
    <path d="M2.5 4.5h11M6 4.5V2.5h4v2M4 4.5l.7 9h6.6l.7-9" />
  </I>
);
export const Play = (p: P) => (
  <I {...p}>
    <path d="M5 3.5v9l7-4.5z" strokeLinejoin="round" />
  </I>
);
export const Stop = (p: P) => (
  <I {...p}>
    <rect x="4" y="4" width="8" height="8" />
  </I>
);
export const Plus = (p: P) => (
  <I {...p}>
    <path d="M8 3v10M3 8h10" />
  </I>
);
export const Folder = (p: P) => (
  <I {...p}>
    <path d="M2 4.5V13h12V5.5H7.5L6 3.5H2z" strokeLinejoin="round" />
  </I>
);
export const File = (p: P) => (
  <I {...p}>
    <path d="M3.5 2h6l3 3v9h-9z M9.5 2v3h3" strokeLinejoin="round" />
  </I>
);
export const External = (p: P) => (
  <I {...p}>
    <path d="M9 3h4v4M13 3 7.5 8.5M11 9.5V13H3V5h3.5" />
  </I>
);
export const Download = (p: P) => (
  <I {...p}>
    <path d="M8 2.5v8M4.5 7 8 10.5 11.5 7M3 13.5h10" />
  </I>
);
export const Refresh = (p: P) => (
  <I {...p}>
    <path d="M13 3v3.5H9.5" />
    <path d="M12.6 6.4A5 5 0 1 0 13 9" />
  </I>
);
export const Sun = (p: P) => (
  <I {...p}>
    <circle cx="8" cy="8" r="2.75" />
    <path d="M8 1.5v1.5M8 13v1.5M1.5 8H3M13 8h1.5M3.4 3.4l1 1M11.6 11.6l1 1M3.4 12.6l1-1M11.6 4.4l1-1" />
  </I>
);
export const Moon = (p: P) => (
  <I {...p}>
    <path d="M13 9.5A5.5 5.5 0 0 1 6.5 3a5.5 5.5 0 1 0 6.5 6.5z" strokeLinejoin="round" />
  </I>
);
export const Grid = (p: P) => (
  <I {...p}>
    <path d="M2.5 2.5h4v4h-4zM9.5 2.5h4v4h-4zM2.5 9.5h4v4h-4zM9.5 9.5h4v4h-4z" />
  </I>
);
export const List = (p: P) => (
  <I {...p}>
    <path d="M5.5 4h8M5.5 8h8M5.5 12h8M2.5 4h.5M2.5 8h.5M2.5 12h.5" />
  </I>
);
export const Sparkle = (p: P) => (
  <I {...p}>
    <path d="M8 2v3.5M8 10.5V14M2 8h3.5M10.5 8H14" />
  </I>
);
export const Terminal = (p: P) => (
  <I {...p}>
    <path d="M3 4.5l3.5 3.5L3 11.5M8 12h5" />
  </I>
);
export const Alert = (p: P) => (
  <I {...p}>
    <path d="M8 2 14.5 13.5h-13z" strokeLinejoin="round" />
    <path d="M8 6.5v3M8 11.5v.5" />
  </I>
);
export const Dots = (p: P) => (
  <I {...p}>
    <path d="M3.5 8h.5M7.75 8h.5M12 8h.5" strokeWidth={2.2} strokeLinecap="round" />
  </I>
);
