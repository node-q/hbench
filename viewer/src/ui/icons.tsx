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
export const CollapseAll = (p: P) => (
  <I {...p}>
    <path d="M5.5 2.5h8v8M2.5 5.5h8v8h-8z M4.75 9.5h3.5" strokeLinejoin="round" />
  </I>
);
export const Files = (p: P) => (
  <I {...p}>
    <path d="M5.5 4.5V1.75h5l2.75 2.75v7.75H10.5 M10.25 1.75V4.75h3" strokeLinejoin="round" />
    <path d="M2.75 4.5h5l2.75 2.75v7H2.75z" strokeLinejoin="round" />
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
export const Server = (p: P) => (
  <I {...p}>
    <rect x="2.5" y="2.5" width="11" height="4.5" rx="1" />
    <rect x="2.5" y="9" width="11" height="4.5" rx="1" />
    <path d="M5 4.75h.01M5 11.25h.01" />
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
export const Code = (p: P) => (
  <I {...p}>
    <path d="M5.5 4.5 2 8l3.5 3.5M10.5 4.5 14 8l-3.5 3.5" />
  </I>
);
export const Shield = (p: P) => (
  <I {...p}>
    <path d="M8 2 13 4v4c0 3-2.2 5-5 6-2.8-1-5-3-5-6V4z" strokeLinejoin="round" />
    <path d="M5.8 8.2 7.4 9.8 10.4 6.5" />
  </I>
);
export const Clipboard = (p: P) => (
  <I {...p}>
    <path d="M5.5 3.5h-2V14h9V3.5h-2" strokeLinejoin="round" />
    <rect x="5.5" y="2" width="5" height="3" />
    <path d="M5.75 8h4.5M5.75 10.75h3" />
  </I>
);
export const Chart = (p: P) => (
  <I {...p}>
    <path d="M2.5 2.5v11h11" />
    <path d="M5 10.5 7.5 7.5l2 2L13 5" />
  </I>
);
export const Layers = (p: P) => (
  <I {...p}>
    <path d="M8 2.5 13.5 5.25 8 8 2.5 5.25z" strokeLinejoin="round" />
    <path d="M2.5 8 8 10.75 13.5 8M2.5 10.75 8 13.5l5.5-2.75" strokeLinejoin="round" />
  </I>
);
export const Branch = (p: P) => (
  <I {...p}>
    <circle cx="4.5" cy="3.75" r="1.5" />
    <circle cx="4.5" cy="12.25" r="1.5" />
    <circle cx="11.5" cy="5.75" r="1.5" />
    <path d="M4.5 5.25v5.5M11.5 7.25c0 2.25-3 2.5-7 2.5" />
  </I>
);
export const Bars = (p: P) => (
  <I {...p}>
    <path d="M3 13.5V9M6.5 13.5V3M10 13.5V6.5M13.5 13.5V10.5" />
  </I>
);
export const Coin = (p: P) => (
  <I {...p}>
    <circle cx="8" cy="8" r="5.75" />
    <path d="M9.9 6.4C9.5 5.8 8.8 5.5 8 5.5c-1.1 0-1.9.55-1.9 1.3 0 1.75 3.8.75 3.8 2.5 0 .7-.8 1.2-1.9 1.2-.9 0-1.6-.3-2-.9M8 4.25V5.5M8 10.5v1.25" strokeLinecap="round" />
  </I>
);
export const Scales = (p: P) => (
  <I {...p}>
    <path d="M8 2.5v11M5 13.5h6M3.5 4.5h9" />
    <path d="M3.5 4.5 1.75 9h3.5zM12.5 4.5 10.75 9h3.5z" strokeLinejoin="round" />
  </I>
);
export const Cross = (p: P) => (
  <I {...p}>
    <path d="M6 2.5h4V6h3.5v4H10v3.5H6V10H2.5V6H6z" strokeLinejoin="round" />
  </I>
);
export const Chat = (p: P) => (
  <I {...p}>
    <path d="M2.5 3h11v8H7.5l-3 2.5V11h-2z" strokeLinejoin="round" />
    <path d="M5.5 6h5M5.5 8.25h3" />
  </I>
);
export const Lock = (p: P) => (
  <I {...p}>
    <rect x="3.5" y="7" width="9" height="6.5" />
    <path d="M5.5 7V5a2.5 2.5 0 0 1 5 0v2M8 9.5v1.5" />
  </I>
);
export const Book = (p: P) => (
  <I {...p}>
    <path d="M8 4.5C6.5 3.5 4.5 3.25 2.5 3.5v9c2-.25 4 0 5.5 1 1.5-1 3.5-1.25 5.5-1v-9c-2-.25-4 0-5.5 1zM8 4.5v9" strokeLinejoin="round" />
  </I>
);
export const Monitor = (p: P) => (
  <I {...p}>
    <rect x="2" y="3" width="12" height="8" />
    <path d="M5.5 13.5h5M8 11v2.5" />
  </I>
);
