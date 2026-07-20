import { createHash } from "node:crypto";

import type { LogLevel } from "../src/contract.js";

export const HF_CACHE_MOUNT_PATH = "/cache/huggingface";
const VLLM_CACHE_SCHEMA = "v1";

const LOG_LINE_BYTES = 4 * 1024;
const LOG_TAIL_BYTES = 8 * 1024;
const LOG_FORWARD_BYTES = 32 * 1024;

type LogSink = (level: LogLevel, message: string) => void;

export interface SandboxOutputCapture {
  failureTail(): string;
  settle(delayMs: number): Promise<void>;
  stop(): Promise<void>;
}

/** One model per Volume limits Modal v1's last-writer-wins conflict surface. */
export function modalCacheVolumeName(model: string): string {
  const modelKey = shortHash(model);
  return `rag-rat-hf-${modelKey}`;
}

export function modalHfReadyMarkerName(model: string, volumeId: string): string {
  return `rag-rat-hf-ready-${shortHash(JSON.stringify({ model, volumeId }))}`;
}

export function modalCacheSandboxName(volumeName: string): string {
  return `rag-rat-cookbook-cache-${shortHash(volumeName)}`;
}

export interface VllmCachePlan {
  readonly rootPath: string;
  readonly markerName: string;
}

/** Namespace compiled code by every provider-level input vLLM's outer AOT key may omit. */
export function vllmCachePlan(
  model: string,
  resolvedImage: string,
  gpu: string,
  capability: string,
  entrypointArgs: readonly string[],
  volumeId: string,
): VllmCachePlan {
  const key = shortHash(
    JSON.stringify({
      schema: VLLM_CACHE_SCHEMA,
      model,
      resolvedImage,
      gpu,
      capability,
      entrypointArgs,
      volumeId,
    }),
  );
  return {
    rootPath: `${HF_CACHE_MOUNT_PATH}/vllm/${key}`,
    markerName: `rag-rat-vllm-ready-${key}`,
  };
}

export function modalCacheEnvironment(vllm: VllmCachePlan | null): Record<string, string> {
  return {
    HF_HOME: HF_CACHE_MOUNT_PATH,
    ...(vllm !== null
      ? {
          VLLM_CACHE_ROOT: vllm.rootPath,
          VLLM_USE_AOT_COMPILE: "1",
          VLLM_USE_MEGA_AOT_ARTIFACT: "0",
        }
      : {}),
  };
}

/**
 * Drain a Sandbox's long-lived main-process streams without ever writing raw bytes to local stdout.
 * Forwarding and retained failure context are independently bounded; draining continues after the
 * forwarding budget is exhausted so the remote process cannot block on a full output pipe.
 */
export function startSandboxOutputCapture(
  stdout: ReadableStream<string>,
  stderr: ReadableStream<string>,
  sink: LogSink,
): SandboxOutputCapture {
  let tail = "";
  let forwardedBytes = 0;
  let suppressionLogged = false;
  const readers: ReadableStreamDefaultReader<string>[] = [];

  const appendTail = (message: string): void => {
    tail = trimUtf8Tail(`${tail}${tail === "" ? "" : "\n"}${message}`, LOG_TAIL_BYTES);
  };

  const forward = (source: "stdout" | "stderr", rawLine: string, truncated: boolean): void => {
    const prefix = `sandbox ${source}: `;
    const cleaned = safeDiagnostic(rawLine, LOG_LINE_BYTES - Buffer.byteLength(prefix), truncated);
    if (cleaned === "" && !truncated) return;
    const message = `${prefix}${cleaned}`;
    appendTail(message);

    const bytes = Buffer.byteLength(message);
    if (forwardedBytes + bytes <= LOG_FORWARD_BYTES) {
      forwardedBytes += bytes;
      sink("info", message);
    } else if (!suppressionLogged) {
      suppressionLogged = true;
      sink("warn", "sandbox output forwarding limit reached; continuing to drain silently");
    }
  };

  const pump = async (stream: ReadableStream<string>, source: "stdout" | "stderr"): Promise<void> => {
    let reader: ReadableStreamDefaultReader<string> | undefined;
    let line = "";
    let lineBytes = 0;
    let lineTruncated = false;

    try {
      reader = stream.getReader();
      readers.push(reader);
      while (true) {
        const { done, value = "" } = await reader.read();
        if (done) break;

        let offset = 0;
        while (offset < value.length) {
          const newline = value.indexOf("\n", offset);
          const end = newline === -1 ? value.length : newline;
          const segment = value.slice(offset, end).replace(/\r$/, "");
          if (!lineTruncated) {
            const room = LOG_LINE_BYTES - lineBytes;
            const kept = takeUtf8Prefix(segment, room);
            line += kept;
            lineBytes += Buffer.byteLength(kept);
            lineTruncated = kept.length !== segment.length;
          }

          if (newline === -1) break;
          forward(source, line, lineTruncated);
          line = "";
          lineBytes = 0;
          lineTruncated = false;
          offset = newline + 1;
        }
      }
      if (line !== "" || lineTruncated) forward(source, line, lineTruncated);
    } catch {
      // Cancellation during teardown and SDK stream failures are both non-fatal diagnostics paths.
    } finally {
      reader?.releaseLock();
    }
  };

  const pumps = [pump(stdout, "stdout"), pump(stderr, "stderr")];

  return {
    failureTail: () => tail,
    settle: async (delayMs) => {
      if (delayMs > 0) await new Promise((resolve) => setTimeout(resolve, delayMs));
    },
    stop: async () => {
      // A terminated Sandbox normally closes both streams. Give already-buffered final diagnostics
      // one event-loop turn to drain before cancellation, while still bounding a live stream.
      await Promise.race([
        Promise.allSettled(pumps),
        new Promise((resolve) => setTimeout(resolve, 25)),
      ]);
      await Promise.allSettled(readers.map((reader) => reader.cancel()));
      await Promise.allSettled(pumps);
    },
  };
}

/** Redact credentials and cap the final, post-redaction diagnostic by UTF-8 bytes. */
export function safeDiagnostic(value: string, maxBytes = LOG_LINE_BYTES, forceTruncated = false): string {
  const cleaned = value
    .replace(/\x1B(?:[@-_][0-?]*[ -/]*[@-~]|\][^\x07]*(?:\x07|\x1B\\))/g, "")
    .replace(/[\x00-\x08\x0B\x0C\x0E-\x1F\x7F]/g, "")
    .replace(
      /\b(Authorization|Proxy-Authorization|X-Api-Key|Api-Key):\s*.*$/gim,
      "$1: [REDACTED]",
    )
    .replace(/\bBearer\s+[^\s]+/gi, "Bearer [REDACTED]")
    .replace(/\bhf_[A-Za-z0-9]{8,}\b/g, "hf_[REDACTED]")
    .replace(
      /([?&](?:access_token|auth_token|token|X-Amz-Credential|X-Amz-Signature|X-Amz-Security-Token|X-Goog-Credential|X-Goog-Signature|Signature|Policy|Key-Pair-Id)=)[^&\s]+/gi,
      "$1[REDACTED]",
    )
    .replace(/(\bhttps?:\/\/)[^/\s@]+@/gi, "$1[REDACTED]@").trimEnd();

  const marker = " [truncated]";
  if (!forceTruncated && Buffer.byteLength(cleaned) <= maxBytes) return cleaned;
  const markerBytes = Buffer.byteLength(marker);
  if (maxBytes <= markerBytes) return takeUtf8Prefix(marker, maxBytes);
  return `${takeUtf8Prefix(cleaned, maxBytes - markerBytes)}${marker}`;
}

function takeUtf8Prefix(value: string, maxBytes: number): string {
  if (maxBytes <= 0) return "";
  const bytes = Buffer.from(value);
  if (bytes.length <= maxBytes) return value;
  return bytes.subarray(0, maxBytes).toString("utf8").replace(/\uFFFD$/, "");
}

function trimUtf8Tail(value: string, maxBytes: number): string {
  const bytes = Buffer.from(value);
  if (bytes.length <= maxBytes) return value;
  return bytes.subarray(bytes.length - maxBytes).toString("utf8").replace(/^\uFFFD/, "");
}

function shortHash(value: string): string {
  return createHash("sha256").update(value).digest("hex").slice(0, 24);
}
