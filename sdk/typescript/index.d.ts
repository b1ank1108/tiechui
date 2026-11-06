import type { Readable, Writable } from "node:stream";
import type { ApprovalDecision, JsonValue, PluginCall, PluginInitialize, PluginOutput, PluginPanel, PluginStage, PluginVerdict } from "./protocol.ts";
export type * from "./protocol.ts";

type Awaitable<T> = T | Promise<T>;
export class RpcError extends Error {
  constructor(code: number, message: string, data?: JsonValue);
  code: number;
  data?: JsonValue;
}
export interface Context {
  readonly signal: AbortSignal;
  progress(progress: number, message: string): Promise<void>;
  request(method: string, params: JsonValue): Promise<JsonValue>;
  notify(title: string, text: string): Promise<void>;
  select(title: string, text: string, options: string[]): Promise<ApprovalDecision>;
  confirm(title: string, text: string): Promise<ApprovalDecision>;
  input(title: string, text: string): Promise<ApprovalDecision>;
  panel(panel: PluginPanel): Promise<ApprovalDecision>;
  status(text: string): Promise<void>;
}
export interface Plugin {
  initialize?(declaration: PluginInitialize, signal: AbortSignal): Awaitable<void>;
  tool?(name: string, arguments_: JsonValue, context: Context): Awaitable<PluginOutput>;
  command?(name: string, arguments_: string, context: Context): Awaitable<string>;
  before?(stage: PluginStage, call: PluginCall, context: Context): Awaitable<PluginVerdict>;
  after?(stage: PluginStage, call: PluginCall, output: JsonValue, context: Context): Awaitable<PluginOutput | null>;
}
export function serve(plugin: Plugin, options: { signal: AbortSignal; input?: Readable; output?: Writable }): Promise<void>;
