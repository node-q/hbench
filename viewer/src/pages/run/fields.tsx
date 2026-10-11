// Form controls for the launcher: label above, helper below (reserved height), error replaces helper after blur.
import { createContext, useContext, useId, useState, type ReactNode } from "react";
import * as Icon from "../../ui/icons";
import type { KwargSpec } from "../../lib/types";
import { kwargError, type KV } from "./form";

/** True after a submit attempt: every field shows its error even if untouched. */
export const ShowAllErrors = createContext(false);

type InputProps = Omit<React.InputHTMLAttributes<HTMLInputElement>, "value" | "onChange">;

export function Field({
  label,
  help,
  error,
  required,
  children,
}: {
  label: ReactNode;
  help?: ReactNode;
  error?: string;
  required?: boolean;
  /** Render prop: spread the a11y props onto the control. */
  children: (p: { id: string; "aria-invalid"?: boolean; "aria-describedby": string; onBlur: () => void }) => ReactNode;
}) {
  const id = useId();
  const [touched, setTouched] = useState(false);
  const showAll = useContext(ShowAllErrors);
  const shown = (touched || showAll) && error;
  return (
    <div className="field">
      <label htmlFor={id}>
        {label}
        {required && (
          <span className="run-req" aria-hidden="true">
            {" "}
            *
          </span>
        )}
      </label>
      {children({ id, "aria-invalid": shown ? true : undefined, "aria-describedby": `${id}-help`, onBlur: () => setTouched(true) })}
      <span id={`${id}-help`} className={`field__help${shown ? " is-error" : ""}`}>
        {shown ? error : help}
      </span>
    </div>
  );
}

export function Text({ label, help, error, required, value, onChange, mono, ...rest }: { label: ReactNode; help?: ReactNode; error?: string; required?: boolean; value: string; onChange: (v: string) => void; mono?: boolean } & InputProps) {
  return (
    <Field label={label} help={help} error={error} required={required}>
      {(p) => <input {...rest} {...p} className={`input${mono ? " input--mono" : ""}`} value={value} onChange={(e) => onChange(e.target.value)} />}
    </Field>
  );
}

export function Num(props: { label: ReactNode; help?: ReactNode; error?: string; value: string; onChange: (v: string) => void; placeholder?: string; step?: string; min?: number }) {
  return <Text {...props} type="number" inputMode="decimal" mono />;
}

export function Area({ label, help, error, value, onChange, placeholder, rows = 3 }: { label: ReactNode; help?: ReactNode; error?: string; value: string; onChange: (v: string) => void; placeholder?: string; rows?: number }) {
  return (
    <Field label={label} help={help} error={error}>
      {(p) => <textarea {...p} className="textarea" rows={rows} value={value} placeholder={placeholder} spellCheck={false} onChange={(e) => onChange(e.target.value)} />}
    </Field>
  );
}

export function Select({ label, help, value, onChange, options }: { label: ReactNode; help?: ReactNode; value: string; onChange: (v: string) => void; options: (string | { value: string; label: string })[] }) {
  return (
    <Field label={label} help={help}>
      {(p) => (
        <select {...p} className="select" value={value} onChange={(e) => onChange(e.target.value)}>
          {options.map((o) => {
            const { value: v, label: l } = typeof o === "string" ? { value: o, label: o } : o;
            return (
              <option key={v} value={v}>
                {l}
              </option>
            );
          })}
        </select>
      )}
    </Field>
  );
}

export function Check({ label, help, checked, onChange }: { label: ReactNode; help?: ReactNode; checked: boolean; onChange: (v: boolean) => void }) {
  return (
    <div className="run-check">
      <label className="check">
        <input type="checkbox" checked={checked} onChange={(e) => onChange(e.target.checked)} />
        {label}
      </label>
      {help && <span className="field__help">{help}</span>}
    </div>
  );
}

/** Collapsible form section (native <details>). */
export function Section({ title, summary, open, children }: { title: string; summary?: ReactNode; open?: boolean; children: ReactNode }) {
  return (
    <details className="run-section" open={open}>
      <summary>
        <Icon.Chevron size={12} className="run-section__chev" />
        <span className="run-section__title">{title}</span>
        {summary && <span className="run-section__summary trunc">{summary}</span>}
      </summary>
      <div className="run-section__body">{children}</div>
    </details>
  );
}

/** Editable KEY / value rows (env vars, custom kwargs). */
export function KvRows({ label, rows, onChange, keyPlaceholder = "KEY", valuePlaceholder = "value", help }: { label: string; rows: KV[]; onChange: (rows: KV[]) => void; keyPlaceholder?: string; valuePlaceholder?: string; help?: ReactNode }) {
  const set = (i: number, patch: Partial<KV>) => onChange(rows.map((r, j) => (j === i ? { ...r, ...patch } : r)));
  return (
    <div className="field run-kv">
      <span className="field__label">{label}</span>
      {rows.map((r, i) => (
        <div key={i} className="run-kv__row">
          <input className="input input--mono" aria-label={`${label} key ${i + 1}`} placeholder={keyPlaceholder} value={r.k} onChange={(e) => set(i, { k: e.target.value })} />
          <input className="input input--mono" aria-label={`${label} value ${i + 1}`} placeholder={valuePlaceholder} value={r.v} onChange={(e) => set(i, { v: e.target.value })} />
          <button type="button" className="btn btn--ghost btn--icon" aria-label={`Remove ${r.k || "row"}`} onClick={() => onChange(rows.filter((_, j) => j !== i))}>
            <Icon.Close size={13} />
          </button>
        </div>
      ))}
      <div>
        <button type="button" className="btn btn--sm" onClick={() => onChange([...rows, { k: "", v: "" }])}>
          <Icon.Plus size={12} /> Add {label.toLowerCase().replace(/s$/, "")}
        </button>
      </div>
      {help && <span className="field__help">{help}</span>}
    </div>
  );
}

/** One declared agent kwarg, control picked by its schema kind. "" = unset (agent default). */
export function KwargField({ spec, value, onChange }: { spec: KwargSpec; value: string; onChange: (v: string) => void }) {
  const def = spec.default == null ? undefined : typeof spec.default === "string" ? spec.default : JSON.stringify(spec.default);
  const label = <span className="mono run-kwarg">{spec.key}</span>;
  const help = spec.description;
  const error = kwargError(spec, value);
  if (spec.kind === "enum" || spec.kind === "bool") {
    const choices = spec.kind === "bool" ? ["true", "false"] : (spec.choices ?? []).map(String);
    return (
      <Field label={label} help={help} required={spec.required} error={error}>
        {(p) => (
          <select {...p} className="select" value={value} onChange={(e) => onChange(e.target.value)}>
            <option value="">{def != null ? `default (${def})` : "default"}</option>
            {choices.map((c) => (
              <option key={c} value={c}>
                {c}
              </option>
            ))}
          </select>
        )}
      </Field>
    );
  }
  if (spec.kind === "json") {
    return <Area label={label} help={help} error={error} value={value} onChange={onChange} placeholder={def ?? '{"key": "value"}'} rows={2} />;
  }
  return (
    <Text
      label={label}
      help={help}
      error={error}
      required={spec.required}
      value={value}
      onChange={onChange}
      placeholder={def ?? (spec.kind === "string" ? "" : "0")}
      mono
      {...(spec.kind === "int" || spec.kind === "float" ? { type: "number", step: spec.kind === "int" ? "1" : "any", inputMode: "decimal" as const } : {})}
    />
  );
}
