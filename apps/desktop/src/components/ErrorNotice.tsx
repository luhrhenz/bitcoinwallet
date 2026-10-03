import type { ReactNode } from "react";
import { describeError } from "../lib/errors";
import type { NetworkName } from "../lib/types";
import { Icon } from "./Icon";

/** A failed command, in plain words (lib/errors.ts). Never a raw message or stack trace. */
export function ErrorNotice({
  error,
  network,
  className,
  children,
}: {
  error: unknown;
  network?: NetworkName;
  className?: string;
  children?: ReactNode;
}) {
  if (error === null || error === undefined) return null;
  const { title, detail } = describeError(error, { network });
  return (
    <div className={`notice notice--error ${className ?? ""}`} role="alert">
      <Icon name="alert" className="notice__icon" />
      <div className="notice__body">
        <p className="notice__title">{title}</p>
        {detail && <p className="notice__detail">{detail}</p>}
        {children}
      </div>
    </div>
  );
}
