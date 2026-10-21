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

export function nearestHistorySample(samples: WeeklyHistorySample[], at: number, interval: HistoryInterval): WeeklyHistorySample | undefined {
  let nearest: WeeklyHistorySample | undefined;
  let distance = Infinity;
  let nearestAt = Infinity;
  for (const sample of samples) {
    const timestamp = Date.parse(sample.fetchedAt);
    if (!(timestamp >= interval.start && timestamp <= interval.end)) continue;
    const difference = Math.abs(timestamp - at);
    if (difference < distance || (difference === distance && timestamp < nearestAt)) {
      nearest = sample;
      distance = difference;
      nearestAt = timestamp;
    }
  }
  return nearest;
}

export function historyPaths(samples: WeeklyHistorySample[], interval: HistoryInterval): { line: string; area: string; gaps: string } {
  const paths = { line: "", area: "", gaps: "" };
  const duration = interval.end - interval.start;
  if (!(duration > 0) || !Number.isFinite(duration)) return paths;
  let segment = "";
  let segmentStart = 0;
  let segmentPoints = 0;
  let previous: { timestamp: number; x: number; y: number; resetsAt: string } | undefined;

  function finishSegment() {
    if (!previous) return;
    paths.line += `${paths.line ? " " : ""}${segment}${segmentPoints === 1 ? ` h ${previous.x <= 999.99 ? ".01" : "-.01"}` : ""}`;
    if (segmentPoints > 1) paths.area += `${paths.area ? " " : ""}${segment} L ${previous.x} 178 L ${segmentStart} 178 Z`;
  }

  for (const sample of samples) {
    const timestamp = Date.parse(sample.fetchedAt);
    if (!(timestamp >= interval.start && timestamp <= interval.end) || !Number.isFinite(sample.usedPercent)) continue;
    const x = (timestamp - interval.start) / duration * 1_000;
    const y = 178 - Math.max(0, Math.min(100, sample.usedPercent)) * 1.76;
    const sameCycle = previous?.resetsAt === sample.resetsAt;
    if (previous && sameCycle && timestamp - previous.timestamp <= 3 * HISTORY_MINUTE) {
      segment += ` L ${x} ${y}`;
      segmentPoints++;
    } else {
      finishSegment();
      if (previous && sameCycle) paths.gaps += `${paths.gaps ? " " : ""}M ${previous.x} ${previous.y} L ${x} ${y}`;
      segment = `M ${x} ${y}`;
      segmentStart = x;
      segmentPoints = 1;
    }
    previous = { timestamp, x, y, resetsAt: sample.resetsAt };
  }
  finishSegment();
  return paths;
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
    { fetchedAt: new Date(start + 2 * HISTORY_MINUTE + 15_000).toISOString(), usedPercent: 25, resetsAt: reset },
    { fetchedAt: new Date(start + 8 * HISTORY_MINUTE + 15_000).toISOString(), usedPercent: 50, resetsAt: reset },
    { fetchedAt: new Date(start + 9 * HISTORY_MINUTE + 15_000).toISOString(), usedPercent: 100, resetsAt: "2026-11-01T01:00:00Z" },
  ];
  const original = JSON.stringify(samples);
  const interval = { start, end: start + 10 * HISTORY_MINUTE };
  check(nearestHistorySample(samples, start + 2 * HISTORY_MINUTE, interval) === samples[1], "snap to the actual nearest timestamp");
  check(nearestHistorySample(samples, start + HISTORY_MINUTE + 15_000, interval) === samples[0], "prefer the earlier timestamp on a tie");
  check(nearestHistorySample(samples, start - HISTORY_MINUTE, interval) === samples[0], "snap to the first record");
  check(nearestHistorySample(samples, interval.end + HISTORY_MINUTE, interval) === samples[3], "snap to the last record");
  check(nearestHistorySample(samples, start, { start: start + HISTORY_MINUTE, end: interval.end }) === samples[1], "exclude earlier out-of-range records");
  check(nearestHistorySample(samples, interval.end, { start, end: start + 8 * HISTORY_MINUTE }) === samples[1], "exclude later out-of-range records");
  check(nearestHistorySample(samples, start, { start: start + 3 * HISTORY_MINUTE, end: start + 4 * HISTORY_MINUTE }) === undefined, "empty selected range");
  check(nearestHistorySample([], start, interval) === undefined, "empty history");
  const recordedAt = Date.parse(samples[1].fetchedAt);
  check(nearestHistorySample(samples, recordedAt, { start: recordedAt, end: recordedAt + HISTORY_MINUTE }) === samples[1], "include the range start");
  check(nearestHistorySample(samples, recordedAt, { start: recordedAt - HISTORY_MINUTE, end: recordedAt }) === samples[1], "include the range end");

  const paths = historyPaths(samples, interval);
  check(paths.line === "M 25 178 L 225 134 M 825 90 h .01 M 925 2 h .01", "connect a 120-second delay but separate long gaps and resets");
  check(paths.gaps === "M 225 134 L 825 90", "only the long same-cycle gap has a separate path");
  check(paths.area === "M 25 178 L 225 134 L 225 178 L 25 178 Z", "fill only solid multi-point segments to the baseline");
  const exactBoundary = { ...samples[1], fetchedAt: new Date(start + 3 * HISTORY_MINUTE + 15_000).toISOString() };
  check(historyPaths([samples[0], exactBoundary], interval).gaps === "", "three minutes is still solid");
  check(historyPaths([samples[0], { ...exactBoundary, fetchedAt: new Date(start + 3 * HISTORY_MINUTE + 15_001).toISOString() }], interval).gaps !== "", "more than three minutes is a gap");
  const isolated = historyPaths(samples, { start: recordedAt - HISTORY_MINUTE, end: recordedAt });
  check(isolated.line === "M 1000 134 h -.01" && isolated.area === "" && isolated.gaps === "", "exclude outside records and show the isolated endpoint without fill");
  check(Object.values(historyPaths([], interval)).every((path) => path === ""), "empty paths");
  check(Object.values(historyPaths(samples, { start: now, end: start })).every((path) => path === ""), "invalid path interval");
  check(JSON.stringify(samples) === original, "do not modify or invent samples");

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
