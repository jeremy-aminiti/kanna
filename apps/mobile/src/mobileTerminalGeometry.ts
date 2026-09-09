// A phone's terminal is sized to what the phone can show. These cell
// dimensions are an estimate of the WebView's rendered grid at its base font;
// the page itself measures the real cell box and reports it back, and that
// measurement replaces this the moment it arrives. This estimate exists only
// for the moments before a page can measure anything: seeding a fresh PTY at
// task creation, and the first frame after navigation.
const ESTIMATED_TERMINAL_CELL_WIDTH_PX = 8;
const ESTIMATED_TERMINAL_CELL_HEIGHT_PX = 17;
const MOBILE_TERMINAL_COMPOSER_AND_CHROME_INSET_PX = 132;
// Below this the estimate came from a viewport that has not laid out yet. A
// grid this small is nobody's terminal, so fall back rather than propose it.
const MINIMUM_MOBILE_TERMINAL_COLS = 20;
const MINIMUM_MOBILE_TERMINAL_ROWS = 8;
// What an unmeasurable viewport gets: the conventional terminal, which is what
// a desktop-shaped session expects before any viewer has said otherwise.
const FALLBACK_MOBILE_TERMINAL_COLS = 80;
const FALLBACK_MOBILE_TERMINAL_ROWS = 24;

export interface MobileTerminalGeometry {
  readonly cols: number;
  readonly rows: number;
}

export interface MobileTerminalViewport {
  width: number;
  height: number;
}

export const DEFAULT_MOBILE_TERMINAL_GEOMETRY: MobileTerminalGeometry =
  Object.freeze({
    cols: FALLBACK_MOBILE_TERMINAL_COLS,
    rows: FALLBACK_MOBILE_TERMINAL_ROWS
  });

export function resolveMobileTerminalGeometry(
  viewport: MobileTerminalViewport | null | undefined
): MobileTerminalGeometry {
  if (
    !viewport ||
    !Number.isFinite(viewport.width) ||
    viewport.width <= 0 ||
    !Number.isFinite(viewport.height) ||
    viewport.height <= 0
  ) {
    return DEFAULT_MOBILE_TERMINAL_GEOMETRY;
  }

  const cols = Math.floor(viewport.width / ESTIMATED_TERMINAL_CELL_WIDTH_PX);
  const rows = Math.floor(
    (viewport.height - MOBILE_TERMINAL_COMPOSER_AND_CHROME_INSET_PX) /
      ESTIMATED_TERMINAL_CELL_HEIGHT_PX
  );
  if (cols < MINIMUM_MOBILE_TERMINAL_COLS || rows < MINIMUM_MOBILE_TERMINAL_ROWS) {
    return DEFAULT_MOBILE_TERMINAL_GEOMETRY;
  }
  return { cols, rows };
}
