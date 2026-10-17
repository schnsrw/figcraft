import { describe, expect, it } from "vitest";

import {
  applyBranding,
  cleanColor,
  cleanText,
  cleanUrl,
  DEFAULT_BRANDING,
  loadBranding,
  MAX_PRODUCT_NAME,
  MAX_SHORT_NAME,
  parseBranding,
} from "./branding";

describe("parseBranding", () => {
  it("returns the defaults for non-objects", () => {
    for (const raw of [null, undefined, 42, "FigCraft", [], true]) {
      expect(parseBranding(raw)).toEqual(DEFAULT_BRANDING);
    }
  });

  it("keeps the defaults for what the default template renders", () => {
    const rendered = {
      version: 1,
      productName: "FigCraft",
      shortName: "FigCraft",
      primaryColor: "#b7df79",
      accentColor: "#94b85c",
      logoUrl: "",
      faviconUrl: "",
      supportUrl: "",
      termsUrl: "",
      privacyUrl: "",
      footerText: "",
    };
    expect(parseBranding(rendered)).toEqual(DEFAULT_BRANDING);
  });

  it("accepts a valid custom brand", () => {
    const b = parseBranding({
      productName: "  Acme   Design ",
      shortName: "Acme",
      primaryColor: "#12AB34",
      accentColor: "#abcdef",
      logoUrl: "/brand/logo.svg",
      faviconUrl: "https://cdn.example.com/favicon.png",
      supportUrl: "https://help.example.com/",
      termsUrl: "/legal/terms",
      privacyUrl: "/legal/privacy?lang=en#top",
      footerText: "© 2026 Acme Inc.",
    });
    expect(b).toEqual({
      productName: "Acme Design",
      shortName: "Acme",
      primaryColor: "#12ab34",
      accentColor: "#abcdef",
      logoUrl: "/brand/logo.svg",
      faviconUrl: "https://cdn.example.com/favicon.png",
      supportUrl: "https://help.example.com/",
      termsUrl: "/legal/terms",
      privacyUrl: "/legal/privacy?lang=en#top",
      footerText: "© 2026 Acme Inc.",
    });
  });

  it("falls back per field and never throws on junk", () => {
    const b = parseBranding({
      productName: "\u0000\u200b\n ",
      shortName: 7,
      primaryColor: "red",
      accentColor: "#12345",
      logoUrl: "javascript:alert(1)",
      faviconUrl: "//evil.example/x.ico",
      supportUrl: "http://plain.example/",
      termsUrl: "data:text/html,hi",
      privacyUrl: "relative/path",
      footerText: { nested: true },
    });
    expect(b).toEqual(DEFAULT_BRANDING);
  });

  it("survives a junk sweep", () => {
    const junk: unknown[] = [
      "",
      " ",
      "\u0000",
      "\\",
      '"',
      "</script>",
      "\ud800",
      "#gggggg",
      "x".repeat(100_000),
      -1,
      NaN,
      Infinity,
      null,
      [],
      {},
      ["#ffffff"],
    ];
    const keys = Object.keys(DEFAULT_BRANDING);
    for (const key of keys) {
      for (const value of junk) {
        const b = parseBranding({ [key]: value });
        expect(b.productName.length).toBeGreaterThan(0);
        expect(Array.from(b.productName).length).toBeLessThanOrEqual(
          MAX_PRODUCT_NAME,
        );
        expect(Array.from(b.shortName).length).toBeLessThanOrEqual(
          MAX_SHORT_NAME,
        );
        expect(b.primaryColor).toMatch(/^#[0-9a-f]{6}$/);
        expect(b.accentColor).toMatch(/^#[0-9a-f]{6}$/);
      }
    }
  });
});

describe("cleanText", () => {
  it("removes control and invisible characters and keeps the literal value", () => {
    expect(cleanText('Acme "Design" \\ </script>\nline2', 64)).toBe(
      'Acme "Design" \\ </script> line2',
    );
    expect(cleanText("A\u202eB\u200dC", 64)).toBe("A B C");
    expect(cleanText("ünï 🎨", 64)).toBe("ünï 🎨");
  });

  it("caps by code points without splitting a surrogate pair", () => {
    expect(cleanText("🎨".repeat(20), MAX_SHORT_NAME)).toBe("🎨".repeat(16));
    expect(cleanText("a".repeat(65), MAX_PRODUCT_NAME)).toBe("a".repeat(64));
  });
});

describe("cleanColor", () => {
  it("accepts only #RRGGBB", () => {
    expect(cleanColor("#A1b2C3")).toBe("#a1b2c3");
    for (const bad of [
      "#abc",
      "a1b2c3",
      "#a1b2c3 ",
      "#a1b2c3ff",
      "rgb(1,2,3)",
      "",
      1,
    ]) {
      expect(cleanColor(bad)).toBeNull();
    }
  });
});

describe("cleanUrl", () => {
  it("accepts same-origin paths and https URLs", () => {
    expect(cleanUrl("/brand/logo.svg")).toBe("/brand/logo.svg");
    expect(cleanUrl("https://example.com/a?b=c")).toBe(
      "https://example.com/a?b=c",
    );
    expect(cleanUrl("HTTPS://Example.com")).toBe("https://example.com/");
  });

  it("rejects everything else", () => {
    for (const bad of [
      "",
      "//evil.example",
      "/\\evil.example",
      "javascript:alert(1)",
      " https://example.com",
      "https://example.com/a b",
      "http://example.com",
      "https://user:pw@example.com/",
      "https://",
      "data:image/svg+xml,<svg/>",
      "brand/logo.svg",
      "/" + "a".repeat(5000),
      null,
      42,
    ]) {
      expect(cleanUrl(bad)).toBeNull();
    }
  });
});

describe("loadBranding", () => {
  const respond =
    (body: string, status = 200) =>
    () =>
      Promise.resolve(new Response(body, { status }));

  it("parses a served document", async () => {
    const b = await loadBranding(
      respond('{"productName":"Acme \\"D\\" \\u003c/script\\u003e"}'),
    );
    expect(b.productName).toBe('Acme "D" </script>');
  });

  it("falls back on errors, bad JSON, HTTP failures and oversized bodies", async () => {
    expect(
      await loadBranding(() => Promise.reject(new Error("offline"))),
    ).toEqual(DEFAULT_BRANDING);
    expect(await loadBranding(respond("{not json"))).toEqual(DEFAULT_BRANDING);
    expect(await loadBranding(respond('{"productName":"X"}', 404))).toEqual(
      DEFAULT_BRANDING,
    );
    expect(
      await loadBranding(
        respond(JSON.stringify({ productName: "x".repeat(20_000) })),
      ),
    ).toEqual(DEFAULT_BRANDING);
  });

  it("falls back when the server does not answer in time", async () => {
    const never = () => new Promise<Response>(() => {});
    expect(await loadBranding(never, "/branding.json", 10)).toEqual(
      DEFAULT_BRANDING,
    );
  });
});

describe("applyBranding", () => {
  function fakeDocument() {
    const props = new Map<string, string>();
    const attrs = new Map<string, string>();
    const appended: { rel: string; href: string }[] = [];
    const meta = { setAttribute: (k: string, v: string) => attrs.set(k, v) };
    const doc = {
      title: "",
      documentElement: {
        style: { setProperty: (k: string, v: string) => props.set(k, v) },
      },
      querySelector: (sel: string) =>
        sel.startsWith('meta[name="description"]') ? meta : null,
      createElement: () => ({ rel: "", href: "" }),
      head: {
        appendChild: (el: { rel: string; href: string }) => appended.push(el),
      },
    };
    return {
      doc: doc as unknown as Document,
      props,
      attrs,
      appended,
      raw: doc,
    };
  }

  it("sets the title, description, colour tokens and favicon", () => {
    const f = fakeDocument();
    applyBranding(f.doc, {
      ...DEFAULT_BRANDING,
      productName: "Acme",
      primaryColor: "#123456",
      faviconUrl: "/brand/favicon.png",
    });
    expect(f.raw.title).toBe("Acme");
    expect(f.attrs.get("content")).toBe("Acme collaborative design workspace");
    expect(f.props.get("--brand-primary")).toBe("#123456");
    expect(f.props.get("--brand-accent")).toBe(DEFAULT_BRANDING.accentColor);
    expect(f.appended).toEqual([{ rel: "icon", href: "/brand/favicon.png" }]);
  });

  it("leaves the favicon alone by default", () => {
    const f = fakeDocument();
    applyBranding(f.doc, { ...DEFAULT_BRANDING });
    expect(f.raw.title).toBe("FigCraft");
    expect(f.appended).toEqual([]);
  });
});
