// @vitest-environment happy-dom

/**
 * Range highlighting against the *real* highlighter.
 *
 * `kanna_open_view` advertises 1-based, inclusive positions counted in Unicode
 * scalars, while Shiki indexes UTF-16 code units and takes an exclusive end.
 * Those two disagreements are invisible to a mocked highlighter — it records
 * whatever offsets it is handed — so this file deliberately does not mock
 * `shiki`, and asserts on the characters that actually end up inside the
 * decoration.
 */
import { mount } from "@vue/test-utils";
import { nextTick } from "vue";
import { afterEach, describe, expect, it, vi } from "vitest";
import FilePreviewModal from "../FilePreviewModal.vue";
import { clearContextShortcuts, resetContext } from "../../composables/useShortcutContext";

const invokeMock = vi.fn<
  (command: string, args?: Record<string, unknown>) => Promise<unknown>
>();

vi.mock("../../invoke", () => ({
  invoke: (...args: [string, Record<string, unknown> | undefined]) => invokeMock(...args),
}));

vi.mock("vue-i18n", () => ({
  useI18n: () => ({ t: (key: string) => key }),
}));

async function flushPromises() {
  await vi.dynamicImportSettled();
  await Promise.resolve();
  await nextTick();
}

/** The text real Shiki put inside the revealed-range decoration. */
function revealedText(html: string): string {
  const host = document.createElement("div");
  host.innerHTML = html;
  return Array.from(host.querySelectorAll(".reveal-hl"))
    .map((element) => element.textContent ?? "")
    .join("");
}

async function reveal(
  content: string,
  target: Record<string, unknown>,
): Promise<{ outcome: { opened: boolean }; text: string }> {
  invokeMock.mockImplementation(async (command) => {
    if (command === "read_text_file") return content;
    return "";
  });
  const wrapper = mount(FilePreviewModal, {
    props: { filePath: "sample.txt", worktreePath: "/repo", embedded: true, active: true },
    attachTo: document.body,
    global: { mocks: { $t: (key: string) => key } },
  });
  // Mounting loads the file, then the highlighter, then renders.
  for (let attempt = 0; attempt < 12; attempt += 1) await flushPromises();

  const outcome = await wrapper.vm.revealDesktopViewTarget({
    requestId: "view-1",
    taskId: "task-a",
    view: "file",
    target: { path: "sample.txt", ...target },
  });
  // A decoration-only change is re-highlighted on a 150ms debounce, so the
  // decorated output is not there on the next microtask.
  await new Promise((resolve) => setTimeout(resolve, 300));
  for (let attempt = 0; attempt < 12; attempt += 1) await flushPromises();

  const html = wrapper.get(".preview-content").element.innerHTML;
  const text = revealedText(html);
  wrapper.unmount();
  return { outcome, text };
}

describe("FilePreviewModal revealed ranges use the advertised units", () => {
  afterEach(() => {
    invokeMock.mockReset();
    clearContextShortcuts();
    resetContext();
    document.body.innerHTML = "";
  });

  it("includes the character the inclusive end names", async () => {
    // column 1 through endColumn 3 of `abc` is `abc`, not `ab`.
    const { outcome, text } = await reveal("abc\n", { line: 1, column: 1, endColumn: 3 });
    expect(outcome.opened).toBe(true);
    expect(text).toBe("abc");
  });

  it("highlights exactly one character for a single-position range", async () => {
    // An inclusive range whose ends meet is one character, never empty.
    const { text } = await reveal("abc\n", { line: 1, column: 1, endColumn: 1 });
    expect(text).toBe("a");
  });

  it("counts scalars, not UTF-16 units, so an emoji is never split", async () => {
    // `😀abc`: column 2 is `a` and column 3 is `b`. Counting UTF-16 units
    // would start inside the surrogate pair and render two broken halves.
    const { text } = await reveal("😀abc\n", { line: 1, column: 2, endColumn: 3 });
    expect(text).toBe("ab");
    expect(text).not.toContain("\uD83D");
    expect(text).not.toContain("\uDE00");
  });

  it("covers a whole non-BMP character when the range starts on it", async () => {
    const { text } = await reveal("😀abc\n", { line: 1, column: 1, endColumn: 1 });
    expect(text).toBe("😀");
  });

  it("spans every line of a multiline range", async () => {
    const { text } = await reveal("one\ntwo\nthree\n", {
      line: 1,
      column: 3,
      endLine: 3,
      endColumn: 2,
    });
    // From `e` on line 1, through all of line 2, to `th` on line 3.
    expect(text).toContain("e");
    expect(text).toContain("two");
    expect(text).toContain("th");
    expect(text).not.toContain("ree");
  });

  it("accepts the end-of-line position one past the last character", async () => {
    const { outcome, text } = await reveal("abc\n", { line: 1, column: 1, endColumn: 4 });
    expect(outcome.opened).toBe(true);
    expect(text).toBe("abc");
  });
});
