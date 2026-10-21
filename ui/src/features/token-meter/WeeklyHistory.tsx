import { CalendarBlank, CaretDown } from "@phosphor-icons/react";
import { useId, useMemo, useRef, useState, type PointerEvent } from "react";
import {
  HISTORY_DURATION,
  HISTORY_MINUTE,
  historyPaths,
  localDateTimeInput,
  nearestHistorySample,
  presetHistoryInterval,
  validHistoryInterval,
  type HistoryInterval,
  type WeeklyHistorySample,
} from "./limitHistory";

type HistoryRange = "1h" | "24h" | "7d" | "custom";

const rangeDate = new Intl.DateTimeFormat(undefined, { month: "short", day: "numeric", hour: "numeric", minute: "2-digit" });
const axisTime = new Intl.DateTimeFormat(undefined, { hour: "numeric", minute: "2-digit" });
const axisDate = new Intl.DateTimeFormat(undefined, { month: "short", day: "numeric" });
const sampleTime = new Intl.DateTimeFormat(undefined, { month: "short", day: "numeric", hour: "numeric", minute: "2-digit", second: "2-digit" });
const timeZone = new Intl.DateTimeFormat(undefined, { timeZoneName: "short" }).formatToParts().find((part) => part.type === "timeZoneName")?.value;

export function WeeklyHistory({ samples, error, now }: { samples: WeeklyHistorySample[]; error?: string; now: number }) {
  const id = useId();
  const [range, setRange] = useState<HistoryRange>("24h");
  const [custom, setCustom] = useState<HistoryInterval>(() => presetHistoryInterval("24h", now));
  const [draftStart, setDraftStart] = useState("");
  const [draftEnd, setDraftEnd] = useState("");
  const [rangeError, setRangeError] = useState<string>();
  const [selected, setSelected] = useState<number | null>(null);
  const [hovered, setHovered] = useState<number | null>(null);
  const [brush, setBrush] = useState<[number, number] | null>(null);
  const drag = useRef<{ x: number; interval: HistoryInterval } | null>(null);
  const interval = range === "custom" ? custom : presetHistoryInterval(range, now);
  const paths = useMemo(() => historyPaths(samples, interval), [samples, interval.start, interval.end]);
  const latest = samples.at(-1);
  const selectedAt = Math.max(interval.start, Math.min(interval.end, hovered ?? selected ?? (latest ? Date.parse(latest.fetchedAt) : interval.end)));
  const sample = nearestHistorySample(samples, selectedAt, interval);
  const guideAt = sample ? Date.parse(sample.fetchedAt) : selectedAt;
  const offset = guideAt - selectedAt;
  const closest = sample && Math.abs(offset) > 90_000 ? `Closest record · ${Math.round(Math.abs(offset) / HISTORY_MINUTE)}m ${offset < 0 ? "earlier" : "later"}` : undefined;
  const position = (at: number) => Math.max(0, Math.min(100, (at - interval.start) / (interval.end - interval.start) * 100));
  const min = localDateTimeInput(Math.floor((now - HISTORY_DURATION) / HISTORY_MINUTE) * HISTORY_MINUTE);
  const max = localDateTimeInput(now);

  function chooseCustom(value: HistoryInterval) {
    setCustom(value);
    setDraftStart(localDateTimeInput(value.start));
    setDraftEnd(localDateTimeInput(value.end));
    setRange("custom");
    setRangeError(undefined);
    setHovered(null);
    setSelected((current) => current == null ? null : Math.max(value.start, Math.min(value.end, current)));
  }

  function pointerPosition(event: PointerEvent<HTMLDivElement>) {
    const bounds = event.currentTarget.getBoundingClientRect();
    return Math.max(0, Math.min(1, (event.clientX - bounds.left) / bounds.width));
  }

  function endDrag(event: PointerEvent<HTMLDivElement>) {
    const gesture = drag.current;
    if (!gesture) return;
    drag.current = null;
    setBrush(null);
    setHovered(null);
    const x = pointerPosition(event);
    const at = (ratio: number) => gesture.interval.start + ratio * (gesture.interval.end - gesture.interval.start);
    if (Math.abs(x - gesture.x) * event.currentTarget.clientWidth < 8) {
      setSelected(at(x));
      return;
    }
    const start = Math.max(gesture.interval.start, Math.floor(at(Math.min(gesture.x, x)) / HISTORY_MINUTE) * HISTORY_MINUTE);
    const end = Math.min(gesture.interval.end, Math.ceil(at(Math.max(gesture.x, x)) / HISTORY_MINUTE) * HISTORY_MINUTE);
    if (validHistoryInterval(start, end, now)) chooseCustom({ start, end });
  }

  return <div className="tm-card tm-history-content">
    <div className="tm-section-heading">
      <div className="tm-history-heading"><span className="tm-label">7 day usage</span>{sample && <output className="tm-history-value" aria-live={hovered == null && !brush ? "polite" : "off"}><strong>{sample.usedPercent}% <span>used</span></strong><time dateTime={sample.fetchedAt}>{sampleTime.format(Date.parse(sample.fetchedAt))}</time></output>}</div>
      <label className="tm-select-control"><CalendarBlank weight="bold" /><span className="tm-sr-only">Usage history range</span><select aria-label="Usage history range" value={range} onChange={(event) => {
        const next = event.target.value as HistoryRange;
        if (next === "custom") chooseCustom(interval);
        else { setRange(next); setSelected(null); setHovered(null); setRangeError(undefined); }
      }}><option value="1h">1h</option><option value="24h">24h</option><option value="7d">7d</option><option value="custom">Custom</option></select><CaretDown className="tm-select-chevron" weight="bold" /></label>
    </div>
    {range === "custom" && <form className="tm-history-range" onSubmit={(event) => {
      event.preventDefault();
      const start = Date.parse(draftStart);
      const end = Date.parse(draftEnd);
      if (!validHistoryInterval(start, end, now)) { setRangeError("Choose at least one minute within the last 7 days."); return; }
      chooseCustom({ start, end });
    }}><label>From<input type="datetime-local" aria-label="History range start" min={min} max={max} step="60" required value={draftStart} onChange={(event) => setDraftStart(event.target.value)} /></label><label>To<input type="datetime-local" aria-label="History range end" min={min} max={max} step="60" required value={draftEnd} onChange={(event) => setDraftEnd(event.target.value)} /></label><button className="tm-button" type="submit">Apply</button>{rangeError && <p className="tm-account-error" role="alert">{rangeError}</p>}</form>}
    <div className="tm-history-interval-row"><p className="tm-history-interval">{rangeDate.format(interval.start)} — {rangeDate.format(interval.end)}{timeZone ? ` · ${timeZone}` : ""}</p>{paths.line && <span className="tm-history-hint">Drag to zoom</span>}</div>
    {error && <p className="tm-account-error" role="status">{error}</p>}
    <div className="tm-chart-layout tm-history-chart-layout">
      <div className="tm-y-axis" aria-hidden="true">{[100, 75, 50, 25, 0].map((percent) => <span style={{ top: `${100 - percent}%` }} key={percent}>{percent}%</span>)}</div>
      <div className="tm-chart tm-history-plot" role="img" aria-label={paths.line ? "7 day usage history. Click to inspect a time or drag to zoom. The inspection time can also be entered below." : "No usage history in this range. Choose a wider range or keep TokenMeter open to collect usage."} onPointerDown={(event) => {
        if (event.button !== 0) return;
        drag.current = { x: pointerPosition(event), interval };
        setHovered(null);
        event.currentTarget.setPointerCapture(event.pointerId);
      }} onPointerMove={(event) => {
        const x = pointerPosition(event);
        if (drag.current) setBrush([drag.current.x, x]);
        else if (event.pointerType === "mouse") setHovered(interval.start + x * (interval.end - interval.start));
      }} onPointerLeave={() => { if (!drag.current) setHovered(null); }} onPointerUp={endDrag} onPointerCancel={() => { drag.current = null; setBrush(null); setHovered(null); }} onLostPointerCapture={() => { drag.current = null; setBrush(null); }}>
        <svg viewBox="0 0 1000 180" preserveAspectRatio="none" aria-hidden="true"><defs><linearGradient id={`${id}-fill`} x1="0" y1="0" x2="0" y2="1"><stop offset="0" stopColor="var(--tm-accent)" stopOpacity=".16" /><stop offset="1" stopColor="var(--tm-accent)" stopOpacity=".01" /></linearGradient></defs><path d={paths.area} fill={`url(#${id}-fill)`} /><path d={paths.gaps} className="tm-history-gap" vectorEffect="non-scaling-stroke" /><path d={paths.line} className="tm-history-line" vectorEffect="non-scaling-stroke" /></svg>
        {!paths.line && <span className="tm-history-empty"><strong>No usage history in this range</strong><span>Choose a wider range or keep TokenMeter open to collect usage.</span></span>}
        {sample && <span className="tm-history-guide" aria-hidden="true" style={{ left: `${position(guideAt)}%` }} />}
        {sample && <i className="tm-history-marker" aria-hidden="true" style={{ left: `${position(guideAt)}%`, top: `${(2 + (100 - sample.usedPercent) * 1.76) / 180 * 100}%` }} />}
        {brush && <span className="tm-history-brush" aria-hidden="true" style={{ left: `${Math.min(...brush) * 100}%`, width: `${Math.abs(brush[1] - brush[0]) * 100}%` }} />}
      </div>
      <div className="tm-x-axis" aria-hidden="true">{[0, .5, 1].map((ratio) => <span style={{ left: `${ratio * 100}%` }} key={ratio}>{(interval.end - interval.start >= 2 * 86_400_000 ? axisDate : axisTime).format(interval.start + ratio * (interval.end - interval.start))}</span>)}</div>
    </div>
    {sample && <div className="tm-history-selection">
      <label htmlFor={`${id}-time`}>Inspect time<input id={`${id}-time`} aria-label="History selected time" type="datetime-local" step="60" min={localDateTimeInput(interval.start)} max={localDateTimeInput(interval.end)} value={localDateTimeInput(selectedAt)} onChange={(event) => {
        const at = Date.parse(event.target.value);
        if (Number.isFinite(at) && at >= Math.floor(interval.start / HISTORY_MINUTE) * HISTORY_MINUTE && at <= interval.end) { setSelected(Math.max(interval.start, at)); setHovered(null); }
      }} /></label>
      {(closest || paths.gaps) && <span className="tm-history-note">{closest}{closest && paths.gaps && <br />}{paths.gaps && "Dashed line: collection gap"}</span>}
    </div>}
  </div>;
}
