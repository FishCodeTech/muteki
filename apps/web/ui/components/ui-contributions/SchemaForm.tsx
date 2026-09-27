"use client";

import { useMemo, useState } from "react";
import { Button, Checkbox, Input, ListBox, ListBoxItem, Select } from "@heroui/react";
import type { CSSProperties } from "react";
import type { ContributionField, JsonSchemaSubset } from "./types";

/**
 * EXT-02：声明式表单渲染器。
 *
 * 两个入口：
 * - `SchemaForm`：按 JSON Schema 子集（type/properties/required/enum）渲染
 *   设置表单，产出 config 对象；
 * - `FieldListForm`：按 ContributionField 列表渲染命令 / 创建表单，
 *   产出 params 对象。
 *
 * 都只渲染宿主内置控件（input/select/checkbox），不执行扩展提供的任何代码。
 */

const row: CSSProperties = { display: "flex", alignItems: "center", gap: 10, flexWrap: "wrap" };
const muted: CSSProperties = { color: "var(--muted)", fontSize: 12 };
const input: CSSProperties = {
  height: 30,
  padding: "0 10px",
  border: "1px solid var(--line2)",
  borderRadius: 8,
  background: "var(--panel2)",
  color: "var(--bright)",
  fontSize: 12,
  minWidth: 0,
};

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

/** 单字段编辑器（schema 子集与 ContributionField 共用的控件选择逻辑）。 */
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
  if (type === "boolean") {
    return (
      <div style={muted}>
        <Checkbox isSelected={value === true} onChange={onChange}>
          <Checkbox.Content><Checkbox.Control><Checkbox.Indicator /></Checkbox.Control>{label}{required ? " *" : ""}</Checkbox.Content>
        </Checkbox>
      </div>
    );
  }
  if (Array.isArray(enumValues) && enumValues.length) {
    return (
      <>
        <label style={muted}>
          {label}
          {required ? " *" : ""}
        </label>
        <Select
          aria-label={label}
          selectedKey={value === undefined || value === null ? "" : String(value)}
          onSelectionChange={(key) => onChange(String(key ?? "") === "" ? undefined : coerce(String(key), type))}
          style={input}
        >
          <Select.Trigger><Select.Value /></Select.Trigger>
          <Select.Popover><ListBox>
            <ListBoxItem id="">（未设置）</ListBoxItem>
            {enumValues.map((v) => (
              <ListBoxItem key={String(v)} id={String(v)}>{String(v)}</ListBoxItem>
            ))}
          </ListBox></Select.Popover>
        </Select>
      </>
    );
  }
  return (
    <>
      <label style={muted}>
        {label}
        {required ? " *" : ""}
      </label>
      <Input
        style={{ ...input, flex: 1 }}
        placeholder={placeholder ?? name}
        value={value === undefined || value === null ? "" : String(value)}
        onChange={(e) => onChange(e.target.value === "" ? undefined : coerce(e.target.value, type))}
      />
    </>
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
    return <div style={muted}>该扩展未声明可配置项。</div>;
  }
  const required = new Set(schema.required ?? []);
  return (
    <div style={{ display: "grid", gap: 8 }}>
      {properties.map(([name, sub]) => (
        <div key={name} style={row}>
          <FieldEditor
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
        </div>
      ))}
      <div style={row}>
        <Button
          type="button"
          style={{
            height: 30,
            padding: "0 12px",
            border: "1px solid color-mix(in srgb, var(--blue) 50%, var(--line2))",
            borderRadius: 8,
            background: "color-mix(in srgb, var(--blue) 14%, var(--panel))",
            color: "var(--bright)",
            fontSize: 12,
            fontWeight: 650,
            cursor: "pointer",
          }}
          isDisabled={busy}
          onClick={() => onSubmit(values)}
        >
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
    <div style={{ display: "grid", gap: 8 }}>
      <div style={row}>
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
        <Button
          type="button"
          style={{
            height: 30,
            padding: "0 12px",
            border: "1px solid color-mix(in srgb, var(--blue) 50%, var(--line2))",
            borderRadius: 8,
            background: "color-mix(in srgb, var(--blue) 14%, var(--panel))",
            color: "var(--bright)",
            fontSize: 12,
            fontWeight: 650,
            cursor: "pointer",
          }}
          isDisabled={busy || missing}
          onClick={() => onSubmit(values)}
        >
          {busy ? "执行中…" : submitLabel}
        </Button>
      </div>
    </div>
  );
}
