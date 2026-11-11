const locale = typeof navigator !== "undefined" ? navigator.language : "en";

const UNITS = ["B", "KB", "MB", "GB", "TB"];

/** 1536 → "1.5 KB". Binary multiples, labelled the way Windows Explorer labels them. */
export function bytes(n: number): string {
  let v = n;
  let u = 0;
  while (v >= 1024 && u < UNITS.length - 1) {
    v /= 1024;
    u++;
  }
  const digits = u === 0 || v >= 100 ? 0 : 1;
  return `${new Intl.NumberFormat(locale, { maximumFractionDigits: digits }).format(v)} ${UNITS[u]}`;
}

export function count(n: number): string {
  return new Intl.NumberFormat(locale).format(n);
}

const rules = new Intl.PluralRules(locale);

/** plural(3, "file", "files") → "3 files". English copy, locale-aware number. */
export function plural(n: number, one: string, other: string): string {
  return `${count(n)} ${rules.select(n) === "one" ? one : other}`;
}

const rtf = new Intl.RelativeTimeFormat(locale, { numeric: "auto" });
const dateFmt = new Intl.DateTimeFormat(locale, { month: "short", day: "numeric" });

export function relative(ms: number, now: number = Date.now()): string {
  const s = Math.round((ms - now) / 1000);
  const abs = Math.abs(s);
  if (abs < 45) return "just now";
  if (abs < 3600) return rtf.format(Math.round(s / 60), "minute");
  if (abs < 86400) return rtf.format(Math.round(s / 3600), "hour");
  if (abs < 7 * 86400) return rtf.format(Math.round(s / 86400), "day");
  return dateFmt.format(ms);
}

export function basename(path: string): string {
  const parts = path.split(/[\/]/).filter(Boolean);
  return parts[parts.length - 1] ?? path;
}

export function dirname(path: string): string {
  const parts = path.split(/[\/]/).filter(Boolean);
  parts.pop();
  return parts.join("/");
}

export const GB = 1024 ** 3;

/** 2_200_000 → "2.1 MB/s". */
export function rate(bps: number): string {
  return `${bytes(bps)}/s`;
}

const num = new Intl.NumberFormat(locale);

/**
 * Time left, rounded the way people say it: "less than a minute", "12 min", "1 h 10 min",
 * "2 days". With `about`, estimates over a minute read "about 25 min".
 */
export function duration(secs: number, opts: { about?: boolean } = {}): string {
  if (secs < 60) return "less than a minute";
  const prefix = opts.about ? "about " : "";
  const mins = Math.round(secs / 60);
  if (mins < 60) return `${prefix}${num.format(mins)} min`;
  if (mins < 48 * 60) {
    // Past an hour, five-minute steps are as precise as the estimate is.
    const step = mins < 10 * 60 ? 5 : 60;
    const rounded = Math.round(mins / step) * step;
    const h = Math.floor(rounded / 60);
    const m = rounded % 60;
    return `${prefix}${num.format(h)} h${m ? ` ${num.format(m)} min` : ""}`;
  }
  return `${prefix}${plural(Math.round(mins / (24 * 60)), "day", "days")}`;
}

const clockFmt = new Intl.DateTimeFormat(locale, { hour: "numeric", minute: "2-digit" });

/** A wall-clock time in the user's format: 1 AM → "01:00" or "1:00 AM". */
export function clock(ms: number): string {
  return clockFmt.format(ms);
}

/** "01:00" (a settings value) → the same time in the user's format. */
export function clockOf(hhmm: string): string {
  const [h, m] = hhmm.split(":").map(Number);
  const d = new Date(2000, 0, 1, h || 0, m || 0);
  return clockFmt.format(d);
}

/** When the overnight window opens, as people say it: "Waiting for tonight (01:00)" or "Waiting until 14:00". */
export function waitingForWindow(opensMs: number): string {
  const h = new Date(opensMs).getHours();
  return h >= 18 || h < 6 ? `Waiting for tonight (${clock(opensMs)})` : `Waiting until ${clock(opensMs)}`;
}
