// Provider presets for the assistant (mirrors src-tauri/src/assistant/settings.rs).

import type { AssistantProvider, AssistantSettings } from "./types";

export interface Preset {
  id: AssistantProvider;
  label: string;
  baseUrl: string;
  model: string;
  /** Where to get a free key. */
  keyHint: string;
}

export const PRESETS: Record<AssistantProvider, Preset> = {
  groq: {
    id: "groq",
    label: "Groq",
    baseUrl: "https://api.groq.com/openai/v1",
    model: "llama-3.3-70b-versatile",
    keyHint: "Free key from console.groq.com",
  },
  gemini: {
    id: "gemini",
    label: "Google Gemini",
    baseUrl: "https://generativelanguage.googleapis.com/v1beta/openai",
    model: "gemini-2.5-flash",
    keyHint: "Free key from aistudio.google.com",
  },
  custom: {
    id: "custom",
    label: "Custom",
    baseUrl: "",
    model: "",
    keyHint: "Any OpenAI-compatible provider with tool calling",
  },
};

export const DEFAULT_ASSISTANT: AssistantSettings = {
  enabled: false,
  consented: false,
  provider: "groq",
  base_url: PRESETS.groq.baseUrl,
  model: PRESETS.groq.model,
  has_api_key: false,
  live_price: false,
};
