/** Where the user is in a fixed sequence (create: password → write down → check). */
export function Steps({ steps, current }: { steps: string[]; current: number }) {
  return (
    <ol className="steps" aria-label="Progress">
      {steps.map((label, i) => (
        <li
          key={label}
          className={i < current ? "steps__item steps__item--done" : i === current ? "steps__item steps__item--current" : "steps__item"}
          aria-current={i === current ? "step" : undefined}
        >
          <span className="steps__n" aria-hidden="true">
            {i + 1}
          </span>
          {label}
        </li>
      ))}
    </ol>
  );
}
