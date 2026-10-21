export type WeeklyHistorySample = {
  fetchedAt: string;
  usedPercent: number;
  resetsAt: string;
};

export type HistoryInterval = { start: number; end: number };

export const HISTORY_MINUTE = 60_000;
export const HISTORY_DURATION = 7 * 86_400_000;

export function presetHistoryInterval(range: "1h" | "24h" | "7d", now: number): HistoryInterval {
  const duration = range === "1h" ? 3_600_000 : range === "24h" ? 86_400_000 : HISTORY_DURATION;
  return { start: Math.floor((now - duration) / HISTORY_MINUTE) * HISTORY_MINUTE, end: now };
}

export function minuteHistorySample(samples: WeeklyHistorySample[], minute: number): WeeklyHistorySample | undefined {
  const selectedMinute = Math.floor(minute / HISTORY_MINUTE);
  for (let index = samples.length - 1; index >= 0; index--) {
    const sampleMinute = Math.floor(Date.parse(samples[index].fetchedAt) / HISTORY_MINUTE);
    if (sampleMinute === selectedMinute) return samples[index];
    if (sampleMinute < selectedMinute) break;
  }
  return undefined;
}

export function historyPath(samples: WeeklyHistorySample[], interval: HistoryInterval): string {
  const duration = interval.end - interval.start;
  if (!(duration > 0) || !Number.isFinite(duration)) return "";
  let path = "";
  let previous: WeeklyHistorySample | undefined;
  let previousMinute: number | undefined;
  for (const sample of samples) {
    const timestamp = Date.parse(sample.fetchedAt);
    if (!(timestamp >= interval.start && timestamp <= interval.end) || !Number.isFinite(sample.usedPercent)) continue;
    const minute = Math.floor(timestamp / HISTORY_MINUTE);
    const x = (timestamp - interval.start) / duration * 1_000;
    const y = 178 - Math.max(0, Math.min(100, sample.usedPercent)) * 1.76;
    if (previousMinute !== undefined && minute === previousMinute + 1 && sample.resetsAt === previous?.resetsAt) {
      path += ` H ${x} V ${y}`;
    } else {
      path += `${path ? " " : ""}M ${x} ${y} h ${x <= 999.99 ? ".01" : "-.01"}`;
    }
    previous = sample;
    previousMinute = minute;
  }
  return path;
}

export function localDateTimeInput(timestamp: number): string {
  return new Date(timestamp - new Date(timestamp).getTimezoneOffset() * HISTORY_MINUTE).toISOString().slice(0, 16);
}

export function validHistoryInterval(start: number, end: number, now: number): boolean {
  const earliest = Math.floor((now - HISTORY_DURATION) / HISTORY_MINUTE) * HISTORY_MINUTE;
  return Number.isFinite(start) && Number.isFinite(end) && Number.isFinite(now)
    && start >= earliest && end <= now && end - start >= HISTORY_MINUTE;
}

const nodeProcess = (globalThis as { process?: { argv: string[] } }).process;
if (nodeProcess?.argv[1]?.replace(/\\/g, "/").endsWith("/limitHistory.ts")) {
  function check(condition: unknown, message: string) {
    if (!condition) throw new Error(`limit history self-check failed: ${message}`);
  }

  const start = Date.parse("2026-10-21T01:00:00Z");
  const now = start + 12 * HISTORY_MINUTE + 34_000;
  for (const [range, duration] of [["1h", 3_600_000], ["24h", 86_400_000], ["7d", HISTORY_DURATION]] as const) {
    const interval = presetHistoryInterval(range, now);
    check(interval.start === Math.floor((now - duration) / HISTORY_MINUTE) * HISTORY_MINUTE && interval.end === now, `${range} preset`);
    check(validHistoryInterval(interval.start, interval.end, now), `${range} preset is valid`);
  }

  const reset = "2026-10-25T01:00:00Z";
  const samples: WeeklyHistorySample[] = [
    { fetchedAt: new Date(start + 15_000).toISOString(), usedPercent: 0, resetsAt: reset },
    { fetchedAt: new Date(start + HISTORY_MINUTE + 15_000).toISOString(), usedPercent: 25, resetsAt: reset },
    { fetchedAt: new Date(start + 3 * HISTORY_MINUTE).toISOString(), usedPercent: 50, resetsAt: reset },
    { fetchedAt: new Date(start + 4 * HISTORY_MINUTE).toISOString(), usedPercent: 100, resetsAt: "2026-11-01T01:00:00Z" },
  ];
  check(minuteHistorySample(samples, start + HISTORY_MINUTE + 59_999) === samples[1], "select the same minute");
  check(minuteHistorySample(samples, start + 2 * HISTORY_MINUTE) === undefined, "do not fill a missing minute");
  check(minuteHistorySample(samples, start - HISTORY_MINUTE) === undefined, "no earlier sample");
  check(minuteHistorySample([], start) === undefined, "empty history");
  const newer = { ...samples[0], fetchedAt: new Date(start + 50_000).toISOString(), usedPercent: 10 };
  check(minuteHistorySample([samples[0], newer], start) === newer, "choose the latest duplicate minute");

  const path = historyPath(samples, { start, end: start + 5 * HISTORY_MINUTE });
  check((path.match(/M /g) ?? []).length === 3, "break paths at missing minutes and resets");
  check(path.startsWith("M 50 178 h .01 H 250 V 134"), "use actual timestamps and step after");
  check(path.includes("M 800 2 h .01"), "100 percent maps to the top");
  check(historyPath([samples[0]], { start, end: start + 5 * HISTORY_MINUTE }).includes("h .01"), "show an isolated sample");
  check(historyPath(samples, { start: start + 2 * HISTORY_MINUTE, end: start + 3 * HISTORY_MINUTE }) === "M 1000 90 h -.01", "exclude samples outside the range and keep the endpoint inside the chart");
  check(historyPath([], { start, end: now }) === "", "empty path");
  check(historyPath(samples, { start: now, end: start }) === "", "invalid path interval");

  const earliest = Math.floor((now - HISTORY_DURATION) / HISTORY_MINUTE) * HISTORY_MINUTE;
  check(validHistoryInterval(earliest, now, now), "accept the seven-day boundary minute");
  check(validHistoryInterval(now - HISTORY_MINUTE, now, now), "accept exactly one minute");
  check(!validHistoryInterval(earliest - 1, now, now), "reject more than seven days");
  check(!validHistoryInterval(start, now + 1, now), "reject the future");
  check(!validHistoryInterval(now, start, now), "reject reversed ranges");
  check(!validHistoryInterval(now - HISTORY_MINUTE + 1, now, now), "reject less than one minute");
  check(!validHistoryInterval(Number.NaN, now, now) && !validHistoryInterval(start, Infinity, now) && !validHistoryInterval(start, now, Infinity), "reject non-finite times");
  check(localDateTimeInput(new Date(2026, 0, 15, 7, 8, 59).getTime()) === "2026-01-15T07:08", "winter local datetime");
  check(localDateTimeInput(new Date(2026, 6, 15, 19, 4).getTime()) === "2026-07-15T19:04", "summer local datetime");
}
