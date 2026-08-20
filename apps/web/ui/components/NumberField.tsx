"use client";

import { useCallback, useEffect, useRef, type KeyboardEvent, type PointerEvent } from "react";
import { Icon } from "@/components/Icon";
import { useT } from "@/lib/i18n";

/**
 * Number field in the Beautiful UI Fine-tune Card style, plus a quiet
 * custom stepper so the control is obviously adjustable.
 */

type NumberFieldProps = {
  value: string | number;
  onChange: (next: string) => void;
  min?: number;
  max?: number;
  step?: number;
  placeholder?: string;
  title?: string;
  suffix?: string;
  scrubLabel?: string;
  className?: string;
  disabled?: boolean;
  allowEmpty?: boolean;
  ariaLabel?: string;
};

function decimalsOf(step: number): number {
  const text = String(step);
  const dot = text.indexOf(".");
  return dot === -1 ? 0 : text.length - dot - 1;
}

function clamp(value: number, min?: number, max?: number): number {
  let next = value;
  if (min != null && Number.isFinite(min)) next = Math.max(min, next);
  if (max != null && Number.isFinite(max)) next = Math.min(max, next);
  return next;
}

function formatNumber(value: number, step: number): string {
  const places = decimalsOf(step);
  return places > 0 ? value.toFixed(places) : String(Math.round(value));
}

function parseLoose(raw: string, fallback: number): number {
  const n = Number(raw);
  return raw.trim() === "" || !Number.isFinite(n) ? fallback : n;
}

function stepFrom(raw: string, delta: number, min: number | undefined, max: number | undefined, step: number, allowEmpty: boolean): string {
  if (allowEmpty && raw.trim() === "" && delta === 0) return "";
  const origin = min ?? 0;
  const current = parseLoose(raw, origin);
  return formatNumber(clamp(current + delta, min, max), step);
}

export function NumberField({
  value,
  onChange,
  min,
  max,
  step = 1,
  placeholder,
  title,
  suffix,
  scrubLabel,
  className,
  disabled,
  allowEmpty = false,
  ariaLabel,
}: NumberFieldProps) {
  const t = useT();
  const text = value === undefined || value === null ? "" : String(value);
  const textRef = useRef(text);
  textRef.current = text;
  const drag = useRef<{ x: number; start: number } | null>(null);
  const holdRef = useRef<number | null>(null);

  const applyDelta = useCallback((delta: number) => {
    if (disabled) return;
    onChange(stepFrom(textRef.current, delta, min, max, step, allowEmpty));
  }, [allowEmpty, disabled, max, min, onChange, step]);

  const stopHold = useCallback(() => {
    if (holdRef.current != null) {
      window.clearTimeout(holdRef.current);
      holdRef.current = null;
    }
  }, []);

  useEffect(() => stopHold, [stopHold]);

  const startHold = (delta: number) => {
    if (disabled) return;
    applyDelta(delta);
    stopHold();
    const tick = (delay: number) => {
      holdRef.current = window.setTimeout(() => {
        applyDelta(delta);
        tick(56);
      }, delay);
    };
    tick(360);
  };

  const onScrubPointerDown = (event: PointerEvent<HTMLSpanElement>) => {
    if (disabled) return;
    event.preventDefault();
    event.currentTarget.setPointerCapture(event.pointerId);
    const origin = min ?? 0;
    drag.current = { x: event.clientX, start: parseLoose(text, origin) };
  };

  const onScrubPointerMove = (event: PointerEvent<HTMLSpanElement>) => {
    if (!drag.current || disabled) return;
    const pixelsPerStep = 8;
    const deltaSteps = Math.round((event.clientX - drag.current.x) / pixelsPerStep);
    onChange(formatNumber(clamp(drag.current.start + deltaSteps * step, min, max), step));
  };

  const onScrubPointerUp = (event: PointerEvent<HTMLSpanElement>) => {
    if (event.currentTarget.hasPointerCapture(event.pointerId)) {
      event.currentTarget.releasePointerCapture(event.pointerId);
    }
    drag.current = null;
  };

  const onScrubKeyDown = (event: KeyboardEvent<HTMLSpanElement>) => {
    if (event.key === "ArrowRight" || event.key === "ArrowUp") {
      event.preventDefault();
      applyDelta(step);
    } else if (event.key === "ArrowLeft" || event.key === "ArrowDown") {
      event.preventDefault();
      applyDelta(-step);
    }
  };

  const numericNow = parseLoose(text, min ?? 0);
  const atMax = max != null && Number.isFinite(max) && numericNow >= max && text.trim() !== "";
  const atMin = min != null && Number.isFinite(min) && numericNow <= min && text.trim() !== "";

  return (
    <div className={`num-field${className ? ` ${className}` : ""}${disabled ? " disabled" : ""}`}>
      {scrubLabel ? (
        <span
          className="num-field-scrub"
          role="slider"
          tabIndex={disabled ? -1 : 0}
          aria-label={scrubLabel}
          aria-valuenow={Number.isFinite(numericNow) ? numericNow : undefined}
          aria-valuemin={min}
          aria-valuemax={max}
          aria-disabled={disabled || undefined}
          onPointerDown={onScrubPointerDown}
          onPointerMove={onScrubPointerMove}
          onPointerUp={onScrubPointerUp}
          onPointerCancel={onScrubPointerUp}
          onKeyDown={onScrubKeyDown}
        >
          {scrubLabel}
        </span>
      ) : null}
      <input
        className="num-field-input"
        inputMode={decimalsOf(step) > 0 ? "decimal" : "numeric"}
        aria-label={ariaLabel}
        title={title}
        placeholder={placeholder}
        disabled={disabled}
        value={text}
        onChange={(event) => onChange(event.target.value)}
        onBlur={() => {
          if (allowEmpty && text.trim() === "") {
            onChange("");
            return;
          }
          onChange(stepFrom(text, 0, min, max, step, allowEmpty));
        }}
        onKeyDown={(event) => {
          if (event.key === "ArrowUp") {
            event.preventDefault();
            applyDelta(step);
          } else if (event.key === "ArrowDown") {
            event.preventDefault();
            applyDelta(-step);
          }
        }}
      />
      {suffix ? <span className="num-field-suffix">{suffix}</span> : null}
      <span className="num-field-stepper" aria-hidden={disabled || undefined}>
        <button
          type="button"
          className="num-field-step up"
          tabIndex={-1}
          disabled={disabled || atMax}
          aria-label={t("num.inc")}
          title={t("num.inc")}
          onMouseDown={(event) => event.preventDefault()}
          onPointerDown={(event) => {
            event.preventDefault();
            startHold(step);
          }}
          onPointerUp={stopHold}
          onPointerLeave={stopHold}
          onPointerCancel={stopHold}
        >
          <Icon name="chevronUp" size={10} />
        </button>
        <button
          type="button"
          className="num-field-step down"
          tabIndex={-1}
          disabled={disabled || atMin}
          aria-label={t("num.dec")}
          title={t("num.dec")}
          onMouseDown={(event) => event.preventDefault()}
          onPointerDown={(event) => {
            event.preventDefault();
            startHold(-step);
          }}
          onPointerUp={stopHold}
          onPointerLeave={stopHold}
          onPointerCancel={stopHold}
        >
          <Icon name="chevronDown" size={10} />
        </button>
      </span>
    </div>
  );
}
