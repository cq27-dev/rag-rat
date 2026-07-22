import assert from "node:assert/strict";
import test from "node:test";

import type { LogLevel } from "../src/contract.js";
import { waitForServerListening } from "../src/contract.js";
import { selectBackendSpec } from "../recipes/backends.mjs";
import {
  HF_CACHE_MOUNT_PATH,
  modalCacheEnvironment,
  modalCacheSandboxName,
  modalCacheVolumeName,
  modalHfReadyMarkerName,
  safeDiagnostic,
  startSandboxOutputCapture,
  vllmCachePlan,
} from "../recipes/modal-support.mjs";

function stream(chunks: readonly string[], stayOpen = false): ReadableStream<string> {
  return new ReadableStream({
    start(controller) {
      for (const chunk of chunks) controller.enqueue(chunk);
      if (!stayOpen) controller.close();
    },
  });
}

test("cache names are stable, model-scoped, and do not expose model ids", () => {
  const model = "private-org/secret-model";
  const name = modalCacheVolumeName(model);
  assert.equal(name, modalCacheVolumeName(model));
  assert.notEqual(name, modalCacheVolumeName("other/model"));
  assert.equal(name.includes(model), false);
  assert.match(name, /^rag-rat-hf-[a-f0-9]{24}$/);
  assert.match(modalHfReadyMarkerName(model, "vo-123"), /^rag-rat-hf-ready-[a-f0-9]{24}$/);
  assert.notEqual(modalHfReadyMarkerName(model, "vo-123"), modalHfReadyMarkerName(model, "vo-456"));
  assert.match(
    modalCacheSandboxName(modalCacheVolumeName(model)),
    /^rag-rat-cookbook-cache-[a-f0-9]{24}$/,
  );
  assert.equal(HF_CACHE_MOUNT_PATH, "/cache/huggingface");
});

test("vLLM cache plans include provider compatibility inputs", () => {
  const args = ["org/model", "--runner", "pooling"];
  const plan = vllmCachePlan("org/model", "image@sha256:one", "A10G", "embed", args, "vo-1");
  assert.deepEqual(
    plan,
    vllmCachePlan("org/model", "image@sha256:one", "A10G", "embed", args, "vo-1"),
  );
  assert.match(plan.rootPath, /^\/cache\/huggingface\/vllm\/[a-f0-9]{24}$/);
  assert.match(plan.markerName, /^rag-rat-vllm-ready-[a-f0-9]{24}$/);
  assert.notEqual(
    plan.markerName,
    vllmCachePlan("org/model", "image@sha256:one", "L4", "embed", ["org/model"], "vo-1")
      .markerName,
  );
  assert.notEqual(
    plan.markerName,
    vllmCachePlan("org/model", "image@sha256:one", "A10G", "embed", args, "vo-2").markerName,
  );
});

test("cache environment enables persistent AOT reuse with normal fallback", () => {
  const plan = vllmCachePlan(
    "org/model",
    "image@sha256:one",
    "A10G",
    "embed",
    ["org/model"],
    "vo-1",
  );
  assert.deepEqual(modalCacheEnvironment(plan), {
    HF_HOME: "/cache/huggingface",
    VLLM_CACHE_ROOT: plan.rootPath,
    VLLM_USE_AOT_COMPILE: "1",
    VLLM_USE_MEGA_AOT_ARTIFACT: "0",
  });
  assert.equal("VLLM_FORCE_AOT_LOAD" in modalCacheEnvironment(plan), false);
});

test("infinity CPU avoids persisting non-portable optimum artifacts", async () => {
  const spec = selectBackendSpec("infinity");
  const cpuArgs = await spec.entrypointArgs({ model: "org/model", backend: "infinity" });
  assert.deepEqual(cpuArgs, [
    "v2",
    "--model-id",
    "org/model",
    "--engine",
    "torch",
    "--device",
    "cpu",
    "--port",
    "7997",
  ]);

  const gpuArgs = await spec.entrypointArgs({ model: "org/model", backend: "infinity", gpu: "A10G" });
  assert.equal(gpuArgs.includes("--engine"), false);
});

test("capture joins chunks, redacts secrets, and retains useful failure context", async () => {
  const events: { level: LogLevel; message: string }[] = [];
  const capture = startSandboxOutputCapture(
    stream(["loading hf_abcd", "efgh1234\nready\n"]),
    stream(["Authorization: Bearer secret-value\n"]),
    (level, message) => events.push({ level, message }),
  );
  await capture.stop();

  assert.deepEqual(
    events.map((event) => event.message).sort(),
    [
      "sandbox stderr: Authorization: [REDACTED]",
      "sandbox stdout: loading hf_[REDACTED]",
      "sandbox stdout: ready",
    ].sort(),
  );
  assert.ok(events.every((event) => event.level === "info"));
  assert.match(capture.failureTail(), /loading hf_\[REDACTED\]/);
  assert.doesNotMatch(capture.failureTail(), /secret-value|hf_abcdefgh1234/);
});

test("capture bounds long lines and forwarding while continuing to drain", async () => {
  const events: { level: LogLevel; message: string }[] = [];
  const chunks = Array.from({ length: 20 }, (_, index) => `${index}:${"x".repeat(5000)}\n`);
  const capture = startSandboxOutputCapture(stream(chunks), stream([]), (level, message) =>
    events.push({ level, message }),
  );
  await capture.stop();

  assert.equal(events.filter((event) => event.level === "warn").length, 1);
  assert.match(events.at(-1)?.message ?? "", /continuing to drain silently/);
  assert.ok(Buffer.byteLength(capture.failureTail()) <= 8 * 1024);
  assert.ok(events.every((event) => Buffer.byteLength(event.message) < 5 * 1024));
});

test("post-redaction diagnostics remain bounded and redact signed URLs", () => {
  const repeatedTokens = Array.from({ length: 300 }, () => "Bearer x").join(" ");
  const bounded = safeDiagnostic(repeatedTokens, 1024);
  assert.ok(Buffer.byteLength(bounded) <= 1024);
  assert.match(bounded, /\[truncated\]$/);
  assert.doesNotMatch(bounded, /Bearer x/);

  const signed = safeDiagnostic(
    "https://user:pass@example.com/model?X-Amz-Credential=abc&X-Amz-Signature=secret Authorization: Basic abc",
  );
  assert.doesNotMatch(signed, /user:pass|=abc|=secret|Basic abc/);
  assert.match(signed, /\[REDACTED\]/);

  const diagnostic = safeDiagnostic(`failure ${"\\\"\t".repeat(5000)}`, 7 * 1024);
  const event = JSON.stringify({ type: "error", message: `sandbox readiness failed: ${diagnostic}`, ts: 0 });
  assert.ok(Buffer.byteLength(event) < 16 * 1024);
});

test("capture can be stopped while long-lived sandbox streams remain open", async () => {
  const capture = startSandboxOutputCapture(stream(["booted\n"], true), stream([], true), () => {});
  await capture.settle(0);
  await capture.stop();
  assert.equal(capture.failureTail(), "sandbox stdout: booted");
});

test("a locked stream cannot create an unhandled pump rejection", async () => {
  const locked = stream([], true);
  const owner = locked.getReader();
  const capture = startSandboxOutputCapture(locked, stream([]), () => {});
  await capture.settle(0);
  await capture.stop();
  await owner.cancel();
  owner.releaseLock();
  assert.equal(capture.failureTail(), "");
});

test("waitForServerListening retries connection-refused and gateway 5xx, then returns on a server answer", async () => {
  const original = globalThis.fetch;
  const responses: Array<() => Promise<Response>> = [
    () => Promise.reject(new Error("ECONNREFUSED")), // server still booting
    () => Promise.resolve(new Response("bad gateway", { status: 502 })), // tunnel up, port not yet
    () => Promise.resolve(new Response("Ollama is running", { status: 200 })), // listening
  ];
  let calls = 0;
  globalThis.fetch = (() => {
    const next = responses[Math.min(calls, responses.length - 1)]!;
    calls += 1;
    return next();
  }) as typeof fetch;
  try {
    await waitForServerListening("https://box.example", { budgetMs: 5_000, pollIntervalMs: 1 });
    assert.equal(calls, 3);
  } finally {
    globalThis.fetch = original;
  }
});

test("waitForServerListening throws when the server never listens within the budget", async () => {
  const original = globalThis.fetch;
  globalThis.fetch = (() => Promise.reject(new Error("ECONNREFUSED"))) as typeof fetch;
  try {
    await assert.rejects(
      () => waitForServerListening("https://box.example", { budgetMs: 30, pollIntervalMs: 1 }),
      /did not start listening/,
    );
  } finally {
    globalThis.fetch = original;
  }
});
