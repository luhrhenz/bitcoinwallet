import { useId, type InputHTMLAttributes, type ReactNode, type Ref } from "react";

interface FieldProps extends Omit<InputHTMLAttributes<HTMLInputElement>, "id"> {
  /** The input element (React 19 passes `ref` to function components as a prop). */
  ref?: Ref<HTMLInputElement>;
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
  const describedBy =
    [hintId, errorId, input["aria-describedby"]].filter(Boolean).join(" ") || undefined;
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
          {...input}
          aria-describedby={describedBy}
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
