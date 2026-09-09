import { describe, expect, it } from "vitest";
import { readFileSync } from "node:fs";
import { resolve } from "node:path";

interface QuotaRejectionCapture {
  provider: string;
  cliVersion: string;
  capturedAt: string;
  capturedFrom: string;
  ruleId: string | null;
  scope: string | null;
  frame?: string[];
  wrappedFrame?: string[];
  parksAs?: string;
  mustNotMatch?: string[];
}

const fixturePath = resolve(
  new URL("../../fixtures/provider-quota-rejection.json", import.meta.url).pathname,
);
const captures = JSON.parse(readFileSync(fixturePath, "utf8")) as QuotaRejectionCapture[];

// The daemon's detection tests read this same file, so a pattern change and the
// chrome it was measured against cannot drift apart. What this suite owns is
// the *provenance*: a quota rejection drives automatic provider recovery, so a
// capture that does not say which CLI version it came from is not evidence.
describe("provider quota-rejection chrome contract", () => {
  it("covers both CLIs Kanna can classify a refusal for", () => {
    const providers = new Set(
      captures.filter((capture) => capture.frame).map((capture) => capture.provider),
    );
    expect([...providers].sort()).toEqual(["claude", "codex"]);
  });

  it("tags every capture with the CLI version and origin it was measured from", () => {
    for (const capture of captures) {
      expect(capture.cliVersion).toMatch(/^\d+\.\d+\.\d+$/);
      expect(capture.capturedAt).toMatch(/^\d{4}-\d{2}-\d{2}$/);
      expect(capture.capturedFrom.trim()).not.toBe("");
    }
  });

  it("captures the narrow-terminal wrap of every rejection sentence", () => {
    // Codex breaks its own sentence mid-clause at 80 columns, so a matcher
    // that only ever saw the wide form would silently stop classifying on a
    // narrow terminal — precisely where a stranded task is hardest to notice.
    for (const capture of captures.filter((entry) => entry.frame)) {
      expect(capture.wrappedFrame?.length ?? 0).toBeGreaterThan(0);
      expect(capture.wrappedFrame).not.toEqual(capture.frame);
    }
  });

  it("records that a refused session parks rather than dying", () => {
    // The whole reason a refusal needs its own signal: the session is still a
    // live, healthy agent afterwards, so no runtime status can carry the fact.
    for (const capture of captures.filter((entry) => entry.frame)) {
      expect(capture.parksAs).toBe("idle");
    }
  });

  it("keeps negatives that must never be classified as a refusal", () => {
    const negatives = captures.flatMap((capture) => capture.mustNotMatch ?? []);
    expect(negatives.length).toBeGreaterThan(0);
    // The Codex banner announcing available resets is the one that matters
    // most: it is about quota, it is drawn by the CLI, and it means the
    // opposite of a refusal.
    expect(negatives).toContain("You have 3 usage limit resets available. Run /usage to use one.");
  });
});
