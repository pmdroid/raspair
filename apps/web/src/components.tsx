import { useId } from "react";
import type { ReactNode } from "react";

export function Artwork({ color = "sage", small = false }: { color?: string; small?: boolean }) {
  return (
    <div className={`artwork ${color} ${small ? "small" : ""}`} aria-hidden="true">
      <span />
      <i />
      <b />
    </div>
  );
}

export function Waveform({
  start = 15,
  end = 100,
  active = true,
}: {
  start?: number;
  end?: number;
  active?: boolean;
}) {
  return (
    <svg
      className={`waveform ${active ? "" : "quiet"}`}
      viewBox="0 0 720 132"
      preserveAspectRatio="none"
    >
      <title>Sample audio waveform with the retained opening highlighted</title>
      <rect x="0" y="0" width={start * 7.2} height="132" fill="var(--warm-soft)" />
      {Array.from({ length: 144 }, (_, i) => {
        const amplitude =
          8 + Math.abs(Math.sin(i * 0.72) * Math.cos(i * 0.09)) * (i < 21 ? 15 : 88);
        return (
          <rect
            key={i}
            x={i * 5 + 1}
            y={(132 - amplitude) / 2}
            width="3"
            height={amplitude}
            rx="1.5"
            fill={i / 1.44 < start || i / 1.44 > end ? "var(--warm)" : "var(--accent)"}
          />
        );
      })}
      <line
        x1={start * 7.2}
        x2={start * 7.2}
        y1="0"
        y2="132"
        stroke="var(--warm)"
        strokeWidth="1.5"
        strokeDasharray="3 3"
      />
    </svg>
  );
}

export function Toggle({
  checked,
  onChange,
  children,
}: {
  checked: boolean;
  onChange: (value: boolean) => void;
  children: ReactNode;
}) {
  return (
    <label className="toggle">
      <span>{children}</span>
      <input
        type="checkbox"
        checked={checked}
        onChange={(event) => onChange(event.target.checked)}
      />
      <span className="switch-track" aria-hidden="true" />
    </label>
  );
}

export function Field({
  label,
  value,
  options,
  onChange,
  hint,
}: {
  label: string;
  value: string;
  options: string[];
  onChange: (value: string) => void;
  hint?: string;
}) {
  const id = useId();
  return (
    <div className="field">
      <label htmlFor={id}>{label}</label>
      <select
        id={id}
        value={value}
        aria-describedby={hint ? `${id}-hint` : undefined}
        onChange={(event) => onChange(event.target.value)}
      >
        {options.map((option) => (
          <option key={option}>{option}</option>
        ))}
      </select>
      {hint && <small id={`${id}-hint`}>{hint}</small>}
    </div>
  );
}
