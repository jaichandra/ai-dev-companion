// The LLM proxy for the service: core/llm-proxy-client.js wired to the
// live config (`llmProxy.*`, read on every call, so Settings changes apply
// at once), the `llmProxy.apiKey` credential and the OS user name. One
// client per process, so the 10-minute detection cache is shared.
import * as os from "os";
import type { Config } from "../config";

export interface LlmChatResult {
  text: string;
  value?: unknown;
  model: string | null;
  usage: { prompt: number | null; completion: number | null } | null;
}

export interface LlmDetection {
  state: "ready" | "no-key" | "unreachable" | "tls" | "application" | "user" | "key-rejected" | "error";
  label: string;
  at: number;
}

export interface LlmProxy {
  chat(opts: {
    system?: string;
    messages: { role: "system" | "user" | "assistant"; content: string }[];
    responseSchema?: Record<string, unknown>;
    maxTokens?: number;
  }): Promise<LlmChatResult>;
  embed(texts: string[]): Promise<number[][]>;
  detect(opts?: { refresh?: boolean }): Promise<LlmDetection>;
  available(): Promise<boolean>;
  settings(): { baseUrl: string; user: string; chatModel: string; embeddingModel: string; allowedModels: string[] };
}

// eslint-disable-next-line @typescript-eslint/no-var-requires
const clientModule = require("./llm-proxy-client.js") as {
  createLlmProxyClient(opts: { getConfig: () => Config; getApiKey: () => string | undefined; osUser: string }): LlmProxy;
};
// eslint-disable-next-line @typescript-eslint/no-var-requires
const credentials = require("./credentials.js") as { getToken(name: string): string | undefined };

function osUserName(): string {
  try {
    return os.userInfo().username;
  } catch {
    return "";
  }
}

let shared: LlmProxy | null = null;

export function llmProxyFor(config: Config): LlmProxy {
  if (!shared) {
    shared = clientModule.createLlmProxyClient({
      getConfig: () => config,
      getApiKey: () => credentials.getToken("llmProxy.apiKey"),
      osUser: osUserName(),
    });
  }
  return shared;
}
