/**
 * The few places the application leaves the page. Kept behind one function so tests can observe
 * a navigation without a real one.
 */
type Assign = (url: string) => void;

let assign: Assign = (url) => window.location.assign(url);

export function goTo(url: string): void {
  assign(url);
}

/** Tests only: replace the navigation, or pass `null` to restore the real one. */
export function setNavigationForTests(replacement: Assign | null): void {
  assign = replacement ?? ((url) => window.location.assign(url));
}
