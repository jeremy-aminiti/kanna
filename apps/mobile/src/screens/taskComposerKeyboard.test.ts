import { describe, expect, it } from "vitest";
import {
  getComposerBottomOffset,
  taskKeyboardEventNames
} from "./taskComposerKeyboard";

describe("getComposerBottomOffset", () => {
  it("keeps the composer at its resting bottom when the keyboard is hidden", () => {
    expect(getComposerBottomOffset(0)).toBe(14);
  });

  it("docks the composer just above the keyboard when the keyboard is visible", () => {
    expect(getComposerBottomOffset(320)).toBe(328);
  });

  it("uses the keyboard events React Native exposes on Android", () => {
    expect(taskKeyboardEventNames("android")).toEqual({
      show: "keyboardDidShow",
      hide: "keyboardDidHide"
    });
    expect(taskKeyboardEventNames("ios")).toEqual({
      show: "keyboardWillShow",
      hide: "keyboardWillHide"
    });
  });
});
