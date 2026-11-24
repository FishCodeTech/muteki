"use client";

import { useId, useMemo, useState } from "react";
import { Button, Checkbox, Input, NativeSelect } from "@muteki/ui";
import type { ContributionField, JsonSchemaSubset } from "./types";


function coerce(raw: string, type: string | undefined): unknown {
  if (type === "integer") {
    const n = parseInt(raw, 10);
    return Number.isNaN(n) ? undefined : n;
  }
  if (type === "number") {
    const n = Number(raw);
    return Number.isNaN(n) ? undefined : n;
  }
  return raw;
}

function FieldEditor({
  name,
  label,
  type,
  required,
  enumValues,
  placeholder,
  value,
  onChange,
}: {
  name: string;
  label: string;
  type?: string;
  required?: boolean;
  enumValues?: unknown[];
  placeholder?: string;
  value: unknown;
  onChange: (value: unknown) => void;
}) {
  const id = useId();
  const caption = <>{label}{required ? " *" : ""}</>;
  if (type === "boolean") {
    return (
      <div className="cx-uic-field is-check">
        <Checkbox checked={value === true} onCheckedChange={onChange} label={caption} />
      </div>
    );
  }
  if (Array.isArray(enumValues) && enumValues.length) {
    return (
      <div className="cx-uic-field">
        <span className="cx-uic-field-label" id={id}>{caption}</span>
        <NativeSelect
          aria-labelledby={id}
          value={value === undefined || value === null ? "" : String(value)}
          onChange={(event) => onChange(event.target.value === "" ? undefined : coerce(event.target.value, type))}
        >
          <option value="">（未设置）</option>
          {enumValues.map((v) => (
            <option key={String(v)} value={String(v)}>{String(v)}</option>
          ))}
        </NativeSelect>
      </div>
    );
  }
  return (
    <div className="cx-uic-field">
      <label className="cx-uic-field-label" htmlFor={id}>{caption}</label>
      <Input
        id={id}
        placeholder={placeholder ?? name}
        value={value === undefined || value === null ? "" : String(value)}
        onChange={(e) => onChange(e.target.value === "" ? undefined : coerce(e.target.value, type))}
      />
    </div>
  );
}

export function SchemaForm({
  schema,
  initial,
  submitLabel,
  busy,
  onSubmit,
}: {
  schema: JsonSchemaSubset;
  initial?: Record<string, unknown>;
  submitLabel: string;
  busy?: boolean;
  onSubmit: (config: Record<string, unknown>) => void;
}) {
  const [values, setValues] = useState<Record<string, unknown>>(initial ?? {});
  const properties = useMemo(
    () => Object.entries(schema.properties ?? {}),
    [schema],
  );
  if (!properties.length) {
    return <p className="cx-uic-muted">该扩展未声明可配置项。</p>;
  }
  const required = new Set(schema.required ?? []);
  return (
    <div className="cx-uic-form">
      <div className="cx-uic-fields">
        {properties.map(([name, sub]) => (
          <FieldEditor
            key={name}
            name={name}
            label={name}
            type={typeof sub.type === "string" ? sub.type : undefined}
            required={required.has(name)}
            enumValues={sub.enum}
            value={values[name]}
            onChange={(v) =>
              setValues((prev) => {
                const next = { ...prev };
                if (v === undefined) delete next[name];
                else next[name] = v;
                return next;
              })
            }
          />
        ))}
      </div>
      <div className="cx-uic-actions">
        <Button type="button" variant="primary" loading={busy} onClick={() => onSubmit(values)}>
          {busy ? "提交中…" : submitLabel}
        </Button>
      </div>
    </div>
  );
}

export function FieldListForm({
  fields,
  submitLabel,
  busy,
  onSubmit,
}: {
  fields: ContributionField[];
  submitLabel: string;
  busy?: boolean;
  onSubmit: (params: Record<string, unknown>) => void;
}) {
  const [values, setValues] = useState<Record<string, unknown>>(() => {
    const init: Record<string, unknown> = {};
    for (const f of fields) {
      if (f.default !== undefined) init[f.name] = f.default;
    }
    return init;
  });
  const missing = fields.some(
    (f) => f.required && (values[f.name] === undefined || values[f.name] === ""),
  );
  return (
    <div className="cx-uic-form">
      <div className="cx-uic-fields">
        {fields.map((f) => (
          <FieldEditor
            key={f.name}
            name={f.name}
            label={f.label ?? f.name}
            type={f.type}
            required={f.required}
            enumValues={f.enum}
            placeholder={f.placeholder}
            value={values[f.name]}
            onChange={(v) =>
              setValues((prev) => {
                const next = { ...prev };
                if (v === undefined) delete next[f.name];
                else next[f.name] = v;
                return next;
              })
            }
          />
        ))}
      </div>
      <div className="cx-uic-actions">
        <Button type="button" variant="primary" loading={busy} disabled={missing} onClick={() => onSubmit(values)}>
          {busy ? "执行中…" : submitLabel}
        </Button>
      </div>
    </div>
  );
}
