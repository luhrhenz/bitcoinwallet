/// <reference types="vite/client" />

interface ImportMetaEnv {
  /** Set by `npm run dev:mock`: use the in-memory backend even inside Tauri. */
  readonly VITE_MOCK?: string;
}
