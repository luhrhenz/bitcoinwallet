import { NO_ASSIST, Field } from "./Field";

/** Same rule as btcw-core's `MIN_PASSWORD_LEN`, counted in characters like Rust's `chars()`. */
export const MIN_PASSWORD_LEN = 8;

export const passwordLength = (password: string) => [...password].length;

const COMMON = /^(password|passw0rd|12345678|123456789|1234567890|qwertyui|qwerty123|bitcoin1?|satoshi1?|letmein1?)$/i;

/** A rough guide, not a guarantee: length counts most, variety a little. 0 = too short. */
export function passwordStrength(password: string): { score: 0 | 1 | 2 | 3 | 4; label: string } {
  const length = passwordLength(password);
  if (length < MIN_PASSWORD_LEN) return { score: 0, label: "Too short" };
  if (COMMON.test(password) || /^(.)\1+$/.test(password)) return { score: 1, label: "Weak: easy to guess" };
  const kinds = [/[a-z]/, /[A-Z]/, /\d/, /[^A-Za-z0-9]/].filter((re) => re.test(password)).length;
  const words = password.trim().split(/\s+/).length;
  let score = length >= 16 || words >= 4 ? 3 : length >= 12 ? 2 : 1;
  if (kinds >= 3 && score < 4) score += 1;
  const labels = ["Too short", "Weak", "Fair", "Good", "Strong"] as const;
  const clamped = Math.min(score, 4) as 1 | 2 | 3 | 4;
  return { score: clamped, label: labels[clamped] };
}

export interface PasswordPairState {
  password: string;
  confirm: string;
  /** Show errors for empty/short fields (after a submit attempt). */
  touched: boolean;
}

export function passwordPairErrors({ password, confirm, touched }: PasswordPairState) {
  const short = passwordLength(password) < MIN_PASSWORD_LEN;
  return {
    password: short && (touched || password.length > 0) ? `Use at least ${MIN_PASSWORD_LEN} characters.` : null,
    // Only complain once the second entry is as long as the first (or on submit), not while typing.
    confirm:
      confirm !== password && (touched || (confirm.length > 0 && [...confirm].length >= passwordLength(password)))
        ? "The passwords don't match."
        : null,
    valid: !short && confirm === password,
  };
}

/** New password, typed twice, with a strength hint. */
export function PasswordFields({
  state,
  onChange,
  disabled,
  purpose,
}: {
  state: PasswordPairState;
  onChange: (next: PasswordPairState) => void;
  disabled?: boolean;
  purpose: string;
}) {
  const errors = passwordPairErrors(state);
  const strength = passwordStrength(state.password);
  return (
    <div className="stack stack--tight">
      <Field
        label="New wallet password"
        type="password"
        value={state.password}
        onChange={(e) => onChange({ ...state, password: e.target.value })}
        error={errors.password}
        disabled={disabled}
        hint={purpose}
        {...NO_ASSIST}
        autoComplete="new-password"
      />
      <div className="strength" data-score={strength.score} aria-live="polite">
        <span className="strength__bars" aria-hidden="true">
          {[1, 2, 3, 4].map((n) => (
            <span key={n} className={n <= strength.score ? "strength__bar strength__bar--on" : "strength__bar"} />
          ))}
        </span>
        <span className="strength__label">
          {state.password ? `Strength: ${strength.label}` : `At least ${MIN_PASSWORD_LEN} characters. A few unrelated words make a strong password.`}
        </span>
      </div>
      <Field
        label="Repeat the password"
        type="password"
        value={state.confirm}
        onChange={(e) => onChange({ ...state, confirm: e.target.value })}
        error={errors.confirm}
        disabled={disabled}
        {...NO_ASSIST}
        autoComplete="new-password"
      />
    </div>
  );
}
