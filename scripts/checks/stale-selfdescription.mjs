#!/usr/bin/env bun
// Reject time-frozen self-description in shipped src/** doc-comments (/// //!).
// A shipped crate must not call itself a scaffold/stub/future work: the code is
// the source of truth for what exists, and a module doc frozen at an earlier
// milestone silently lies to every reader. Status lives in one tracked place,
// not scattered across module headers (constitution §5; docs/lessons.md).
//   default : scan STAGED src/**/*.rs (git pre-commit, via lefthook)
//   --all   : scan all tracked src/**/*.rs sources (CI structural gate)
//
// A genuine, still-accurate deferral is allowed through an inline or immediately
// preceding `// stale-ok: <reason >= 12 chars>`; a bare `// stale-ok:` is itself
// an error. Every honoured suppression is echoed to stderr so the CI no-warnings
// log sweep keeps deliberate deferrals visible and auditable.
import { $ } from "bun";
import { stagedFiles } from "../lib/git.mjs";
import { die, notice } from "../lib/ci.mjs";

// PROTECTED ratchet (constitution §7): these arrays only grow. Removing a
// pattern needs a human. Phrase-anchored (not bare words) so ordinary impl prose
// — "narrow does not consume", "lands in [InSourceIdent]" — never trips them.
export const BANNED = [
  /\b(scaffold(ing)?|skeleton)\b/i,
  /\bstub\b|\bStubDecoder\b/i,
  /\bstill absent\b/i,
  /\bnot \(?yet\)? (built|implemented|wired|acted on|narrowed|enumerat\w+)\b/i,
  /\b(later|future|next) (milestone|task|release)\b/i,
  /\blands? in a (later|future)\b|\barrives? at M[0-5]\b/i,
  /\bM0[\s-]*(only|scaffold|skeleton)\b/i,
  /\bwill be (built|added|supplied|wired|implemented)\b|\bto be built\b/i,
  /\b(TODO|FIXME|XXX)\b/, // doc-comment TODOs the macro/comment gate can't see
];

// Only banned inside a `//!` module header, where they describe the module
// itself as throwaway/provisional. A `///` item doc may legitimately say a
// buffer is a "throwaway stack" (accurate current behaviour), so those are only
// self-description smells at the module-header altitude.
export const HEADER_BANNED = [/\bfor now\b/i, /\bthrowaway\b/i];

// A justified suppression: `// stale-ok: <reason>`. The reason must be real
// (>= this many chars) or the marker is itself an error — an empty escape hatch
// is no better than silence.
const SUPPRESS = /\/\/\s*stale-ok:\s*(.*)$/;
export const MIN_REASON_LEN = 12;

/** The trimmed stale-ok reason on `line`, or null if it carries no marker. */
function markerReason(line) {
  if (line === undefined) return null;
  const m = line.match(SUPPRESS);
  return m ? m[1].trim() : null;
}

/**
 * Scan one file's text. Pure and unit-tested — no I/O, no process exit.
 * @param {string} text
 * @returns {{hits: Array<{line:number,text:string,pattern:string}>,
 *            suppressions: Array<{line:number,reason:string}>}}
 */
export function scan(text) {
  const lines = text.split("\n");
  const hits = [];
  const suppressions = [];

  lines.forEach((raw, i) => {
    const t = raw.trimStart();
    const isDoc = t.startsWith("///") || t.startsWith("//!");
    if (!isDoc) return;
    const patterns = t.startsWith("//!") ? [...BANNED, ...HEADER_BANNED] : BANNED;
    const pattern = patterns.find((re) => re.test(raw));
    if (!pattern) return;

    // A justified suppression on the same or immediately preceding line lets a
    // genuine, still-accurate deferral through.
    const here = markerReason(raw);
    const above = markerReason(lines[i - 1]);
    const reason = [here, above].find((r) => r !== null && r.length >= MIN_REASON_LEN);
    if (reason !== undefined) {
      suppressions.push({ line: i + 1, reason });
      return;
    }
    hits.push({ line: i + 1, text: t, pattern: String(pattern) });
  });

  // A bare or too-short `// stale-ok:` is itself an error: the escape hatch must
  // always carry a real justification, wherever it appears.
  lines.forEach((raw, i) => {
    const reason = markerReason(raw);
    if (reason !== null && reason.length < MIN_REASON_LEN) {
      hits.push({
        line: i + 1,
        text: raw.trim(),
        pattern: `bare stale-ok (reason must be >= ${MIN_REASON_LEN} chars)`,
      });
    }
  });

  return { hits, suppressions };
}

async function srcRustFiles() {
  const out = await $`git ls-files -- src`.text();
  return out.split("\n").filter((f) => f.endsWith(".rs"));
}

async function filesToScan() {
  if (process.argv.includes("--all")) return srcRustFiles();
  const staged = await stagedFiles({ suffix: ".rs" });
  return staged.filter((f) => f.startsWith("src/"));
}

const files = await filesToScan();
const allHits = [];
for (const path of files) {
  const { hits, suppressions } = scan(await Bun.file(path).text());
  for (const s of suppressions) notice(`stale-ok honoured ${path}:${s.line}: ${s.reason}`);
  for (const h of hits) allHits.push(`${path}:${h.line}: ${h.text}  [${h.pattern}]`);
}

if (allHits.length) {
  die(
    `stale self-description in shipped doc-comments — reword to present-tense fact, ` +
      `or justify a genuine deferral with an inline \`// stale-ok: <reason>\`:\n${allHits
        .map((h) => `    ${h}`)
        .join("\n")}`,
  );
}
