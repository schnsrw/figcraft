import { cleanup } from "@testing-library/react";
import { afterEach } from "vitest";

afterEach(() => {
  cleanup();
  window.history.replaceState(null, "", "/");
  window.localStorage.clear();
  window.sessionStorage.clear();
});
