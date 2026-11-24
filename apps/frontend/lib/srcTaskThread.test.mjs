import assert from "node:assert/strict";
import test from "node:test";
import { srcTaskThreadId } from "./srcTypes.ts";

test("historical SRC tasks remain readable without a binding record", () => {
  const imported = { task_id: "imported-task", thread_id: "imported-thread", binding: null };
  assert.equal(srcTaskThreadId(imported), "imported-thread");
  assert.equal(srcTaskThreadId({ ...imported, binding: { thread_id: "bound-thread" } }), "bound-thread");
  assert.equal(srcTaskThreadId({ ...imported, thread_id: undefined }), "");
});
