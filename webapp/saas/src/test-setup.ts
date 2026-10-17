import { cleanup, configure } from "@testing-library/react";
import { afterEach } from "vitest";

// Generous: the suite runs many jsdom pages in parallel on busy machines.
configure({ asyncUtilTimeout: 5000 });

afterEach(() => {
  cleanup();
  window.history.replaceState(null, "", "/");
  window.localStorage.clear();
  window.sessionStorage.clear();
});
