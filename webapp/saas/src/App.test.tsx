import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";

import { App } from "./App";

describe("App", () => {
  it("renders the workspace shell and editor boundary status", () => {
    const markup = renderToStaticMarkup(<App />);

    expect(markup).toContain("<h1>Your design workspace</h1>");
    expect(markup).toContain('aria-label="Workspace"');
    expect(markup).toContain("Rust/WASM boundary preserved");
  });
});
