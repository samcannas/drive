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
