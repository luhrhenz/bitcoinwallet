import { useId, type InputHTMLAttributes, type ReactNode } from "react";

interface FieldProps extends Omit<InputHTMLAttributes<HTMLInputElement>, "id"> {
  label: ReactNode;
  hint?: ReactNode;
  error?: string | null;
  /** Rendered right after the input, inside the same row (unit toggles, buttons). */
  addon?: ReactNode;
  mono?: boolean;
}

/** Label + input + hint + error, wired together for screen readers. */
export function Field({ label, hint, error, addon, mono, className, ...input }: FieldProps) {
  const id = useId();
  const hintId = hint ? `${id}-hint` : undefined;
  const errorId = error ? `${id}-error` : undefined;
  const describedBy = [hintId, errorId].filter(Boolean).join(" ") || undefined;
  return (
    <div className={`field ${error ? "field--invalid" : ""} ${className ?? ""}`}>
      <label className="field__label" htmlFor={id}>
        {label}
      </label>
      <div className="field__row">
        <input
          id={id}
          className={`input ${mono ? "input--mono" : ""}`}
          aria-invalid={error ? true : undefined}
          aria-describedby={describedBy}
          {...input}
        />
        {addon}
      </div>
      {hint && (
        <p className="field__hint" id={hintId}>
          {hint}
        </p>
      )}
      {error && (
        <p className="field__error" id={errorId}>
          {error}
        </p>
      )}
    </div>
  );
}

/** Attributes for any input that may receive secrets or Bitcoin data: no autofill, no spellcheck. */
export const NO_ASSIST = {
  autoComplete: "off",
  autoCorrect: "off",
  autoCapitalize: "off",
  spellCheck: false,
  "data-gramm": "false",
} as const;
