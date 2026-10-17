import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";

import { App } from "./App";
import { DEFAULT_BRANDING, parseBranding } from "./branding";

describe("App", () => {
  it("renders the workspace shell and editor boundary status", () => {
    const markup = renderToStaticMarkup(<App />);

    expect(markup).toContain("<h1>Your design workspace</h1>");
    expect(markup).toContain('aria-label="Workspace"');
    expect(markup).toContain("Rust/WASM boundary preserved");
  });

  it("renders the default brand without a footer", () => {
    const markup = renderToStaticMarkup(<App branding={DEFAULT_BRANDING} />);

    expect(markup).toContain("<span>FigCraft</span>");
    expect(markup).toContain('aria-label="FigCraft workspace home"');
    expect(markup).toContain('class="brand-mark"');
    expect(markup).not.toContain("<footer");
  });

  it("renders a validated custom brand, escaping the name", () => {
    const branding = parseBranding({
      productName: 'Acme "Design" </script>',
      logoUrl: "/brand/logo.svg",
      supportUrl: "https://help.example.com/",
      privacyUrl: "javascript:alert(1)",
      footerText: "© Acme",
    });
    const markup = renderToStaticMarkup(<App branding={branding} />);

    expect(markup).toContain(
      "<span>Acme &quot;Design&quot; &lt;/script&gt;</span>",
    );
    expect(markup).not.toContain("</script>");
    expect(markup).toContain('src="/brand/logo.svg"');
    expect(markup).toContain('href="https://help.example.com/"');
    expect(markup).not.toContain("javascript:");
    expect(markup).not.toContain(">Privacy<");
    expect(markup).toContain("© Acme");
  });
});
