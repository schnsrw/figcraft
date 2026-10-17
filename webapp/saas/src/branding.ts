/**
 * Runtime branding: the web container renders `/branding.json` from its FIGCRAFT_BRAND_*
 * environment (deploy/saas/branding.json.tmpl). Nothing in that document is trusted: every field
 * is validated here and anything invalid falls back to that field's default, so a bad value can
 * never break the page. See docs/saas/branding-and-configuration.md.
 */

export interface Branding {
  productName: string;
  shortName: string;
  /** `#RRGGBB`. */
  primaryColor: string;
  /** `#RRGGBB`. */
  accentColor: string;
  /** Same-origin path or `https://` URL; `null` keeps the built-in mark. */
  logoUrl: string | null;
  faviconUrl: string | null;
  supportUrl: string | null;
  termsUrl: string | null;
  privacyUrl: string | null;
  footerText: string | null;
}

export const DEFAULT_BRANDING: Readonly<Branding> = Object.freeze({
  productName: "FigCraft",
  shortName: "FigCraft",
  primaryColor: "#b7df79",
  accentColor: "#94b85c",
  logoUrl: null,
  faviconUrl: null,
  supportUrl: null,
  termsUrl: null,
  privacyUrl: null,
  footerText: null,
});

export const MAX_PRODUCT_NAME = 64;
export const MAX_SHORT_NAME = 16;
export const MAX_FOOTER_TEXT = 200;
export const MAX_URL = 2048;
/** `/branding.json` is a few hundred bytes; anything far larger is not ours. */
export const MAX_BRANDING_BYTES = 16 * 1024;

const COLOR = /^#[0-9a-fA-F]{6}$/;

/**
 * Removes control and invisible formatting characters (C0/C1, bidi overrides, zero-width), folds
 * whitespace runs into one space, trims, and caps the length in code points. `null` when nothing
 * remains or the value is not a string.
 */
export function cleanText(value: unknown, max: number): string | null {
  if (typeof value !== "string") return null;
  const cleaned = value
    .replace(/[\p{Cc}\p{Cf}\p{Zl}\p{Zp}]/gu, " ")
    .replace(/\s+/gu, " ")
    .trim();
  if (cleaned === "") return null;
  const capped = Array.from(cleaned).slice(0, max).join("").trim();
  return capped === "" ? null : capped;
}

export function cleanColor(value: unknown): string | null {
  return typeof value === "string" && COLOR.test(value)
    ? value.toLowerCase()
    : null;
}

/**
 * A same-origin absolute path (`/brand/logo.svg`, never `//host`) or an `https://` URL. Anything
 * else (`javascript:`, `data:`, `http:`, relative paths, whitespace, backslashes, credentials)
 * is rejected.
 */
export function cleanUrl(value: unknown): string | null {
  if (typeof value !== "string" || value === "" || value.length > MAX_URL)
    return null;
  // No whitespace, controls or backslashes anywhere (browsers rewrite `\` to `/`).
  if (/[\s\p{Cc}\p{Cf}\\]/u.test(value)) return null;
  if (value.startsWith("/")) {
    return value.startsWith("//") ? null : value;
  }
  if (!value.toLowerCase().startsWith("https://")) return null;
  let url: URL;
  try {
    url = new URL(value);
  } catch {
    return null;
  }
  if (
    url.protocol !== "https:" ||
    url.hostname === "" ||
    url.username !== "" ||
    url.password !== ""
  ) {
    return null;
  }
  return url.href;
}

/** Validates an untrusted `/branding.json` document field by field. */
export function parseBranding(raw: unknown): Branding {
  const d = DEFAULT_BRANDING;
  if (typeof raw !== "object" || raw === null || Array.isArray(raw))
    return { ...d };
  const r = raw as Record<string, unknown>;
  return {
    productName: cleanText(r.productName, MAX_PRODUCT_NAME) ?? d.productName,
    shortName: cleanText(r.shortName, MAX_SHORT_NAME) ?? d.shortName,
    primaryColor: cleanColor(r.primaryColor) ?? d.primaryColor,
    accentColor: cleanColor(r.accentColor) ?? d.accentColor,
    logoUrl: cleanUrl(r.logoUrl),
    faviconUrl: cleanUrl(r.faviconUrl),
    supportUrl: cleanUrl(r.supportUrl),
    termsUrl: cleanUrl(r.termsUrl),
    privacyUrl: cleanUrl(r.privacyUrl),
    footerText: cleanText(r.footerText, MAX_FOOTER_TEXT),
  };
}

export type FetchLike = (
  input: string,
  init?: RequestInit,
) => Promise<Response>;

/**
 * Fetches and validates `/branding.json`; any failure, or no answer within `timeoutMs`, yields
 * the defaults. Never rejects.
 */
export async function loadBranding(
  fetchFn: FetchLike,
  url = "/branding.json",
  timeoutMs = 2000,
): Promise<Branding> {
  const controller = new AbortController();
  let timer: ReturnType<typeof setTimeout> | undefined;
  const timeout = new Promise<Branding>((resolve) => {
    timer = setTimeout(() => {
      controller.abort();
      resolve({ ...DEFAULT_BRANDING });
    }, timeoutMs);
  });
  const load = (async (): Promise<Branding> => {
    try {
      const response = await fetchFn(url, {
        cache: "no-cache",
        credentials: "same-origin",
        signal: controller.signal,
      });
      if (!response.ok) return { ...DEFAULT_BRANDING };
      const text = await response.text();
      if (text.length > MAX_BRANDING_BYTES) return { ...DEFAULT_BRANDING };
      return parseBranding(JSON.parse(text));
    } catch {
      return { ...DEFAULT_BRANDING };
    }
  })();
  try {
    return await Promise.race([load, timeout]);
  } finally {
    clearTimeout(timer);
  }
}

/** Applies the document-level parts: title, description, colour tokens and favicon. */
export function applyBranding(doc: Document, b: Branding): void {
  doc.title = b.productName;
  doc
    .querySelector('meta[name="description"]')
    ?.setAttribute(
      "content",
      `${b.productName} collaborative design workspace`,
    );
  doc
    .querySelector('meta[name="application-name"]')
    ?.setAttribute("content", b.shortName);
  const root = doc.documentElement.style;
  root.setProperty("--brand-primary", b.primaryColor);
  root.setProperty("--brand-accent", b.accentColor);
  if (b.faviconUrl !== null) {
    let link = doc.querySelector<HTMLLinkElement>('link[rel="icon"]');
    if (link === null) {
      link = doc.createElement("link");
      link.rel = "icon";
      doc.head.appendChild(link);
    }
    link.href = b.faviconUrl;
  }
}
