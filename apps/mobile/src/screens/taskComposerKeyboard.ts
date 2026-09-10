const RESTING_COMPOSER_BOTTOM = 14;
const KEYBOARD_COMPOSER_GAP = 8;

export function taskKeyboardEventNames(platform: string): {
  show: "keyboardWillShow" | "keyboardDidShow";
  hide: "keyboardWillHide" | "keyboardDidHide";
} {
  return platform === "android"
    ? { show: "keyboardDidShow", hide: "keyboardDidHide" }
    : { show: "keyboardWillShow", hide: "keyboardWillHide" };
}

export function getComposerBottomOffset(keyboardHeight: number): number {
  if (keyboardHeight <= 0) {
    return RESTING_COMPOSER_BOTTOM;
  }

  return keyboardHeight + KEYBOARD_COMPOSER_GAP;
}
