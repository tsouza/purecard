// Unit tests for the pure `scan` export of the stale-selfdescription gate.
import { expect, test } from "bun:test";
import { scan } from "./stale-selfdescription.mjs";

test("flags a banned phrase in a /// doc-comment", () => {
  const { hits } = scan("/// This is a stub for now.\npub fn f() {}\n");
  expect(hits).toHaveLength(1);
  expect(hits[0].line).toBe(1);
});

test("flags a banned phrase in a //! module header", () => {
  const { hits } = scan("//! An M0 scaffold that will be built later.\n");
  expect(hits.length).toBeGreaterThan(0);
});

test("ignores banned words in ordinary // impl comments and code", () => {
  const src = [
    "// a stub helper, scaffold only — an impl comment, not a doc-comment",
    'let msg = "not yet complete";',
    "fn stub_helper() {}",
  ].join("\n");
  expect(scan(src).hits).toHaveLength(0);
});

test("does not flag legitimate present-tense doc prose", () => {
  const src = [
    "/// The narrow does not consume the byte; it only clears mask bits.",
    "/// Reuses a `scratch` throwaway stack while probing — current behaviour.",
  ].join("\n");
  // `throwaway` is only banned in //! headers, not /// item docs.
  expect(scan(src).hits).toHaveLength(0);
});

test("banned `throwaway` in a /// item doc is allowed (header-only rule)", () => {
  expect(scan("/// reuses a throwaway stack\n").hits).toHaveLength(0);
});

test("`throwaway` IS flagged in a //! module header", () => {
  expect(scan("//! a throwaway module\n").hits).toHaveLength(1);
});

test("an inline stale-ok with a real reason suppresses the hit", () => {
  const src = "/// **Stub.** ignores spec. // stale-ok: EBNF ingestion deferred by design\n";
  const { hits, suppressions } = scan(src);
  expect(hits).toHaveLength(0);
  expect(suppressions).toHaveLength(1);
  expect(suppressions[0].reason).toContain("EBNF");
});

test("a stale-ok on the immediately preceding line suppresses the hit", () => {
  const src = ["// stale-ok: genuinely deferred until the engine lands", "/// a stub for now\n"].join(
    "\n",
  );
  const { hits, suppressions } = scan(src);
  expect(hits).toHaveLength(0);
  expect(suppressions).toHaveLength(1);
});

test("a bare stale-ok is itself an error", () => {
  const { hits } = scan("/// a stub. // stale-ok:\n");
  // The hit is not suppressed AND the bare marker is reported.
  expect(hits.some((h) => h.pattern.includes("bare stale-ok"))).toBe(true);
});

test("a too-short stale-ok reason does not suppress and is itself an error", () => {
  const { hits } = scan("/// a stub // stale-ok: short\n");
  expect(hits.length).toBeGreaterThan(0);
  expect(hits.some((h) => h.pattern.includes("bare stale-ok"))).toBe(true);
});

test("TODO/FIXME/XXX in a doc-comment are flagged", () => {
  expect(scan("/// TODO: wire this up\n").hits).toHaveLength(1);
  expect(scan("/// FIXME later\n").hits).toHaveLength(1);
  expect(scan("/// XXX broken\n").hits).toHaveLength(1);
});

test("clean doc-comments produce no hits", () => {
  const src = [
    "//! # PureCard",
    "//! A grammar- and schema-constrained decoder for Legend Pure.",
    "/// Build a vocabulary from a list of token byte-strings.",
  ].join("\n");
  expect(scan(src).hits).toHaveLength(0);
});
