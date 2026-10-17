import { describe, expect, it } from "vitest";

import {
  MAX_RETURN_TO,
  formatRoute,
  parseRoute,
  safeReturnTo,
  signInHref,
} from "./router";
import { DOCUMENT_ID, WORKSPACE_ID } from "./testing";

describe("parseRoute", () => {
  it("recognises the stable URLs", () => {
    expect(parseRoute("/")).toEqual({ name: "home" });
    expect(parseRoute("/people")).toEqual({ name: "people" });
    expect(parseRoute("/invite")).toEqual({ name: "invite" });
    expect(parseRoute(`/workspace/${WORKSPACE_ID}`)).toEqual({
      name: "workspace",
      workspaceId: WORKSPACE_ID,
    });
    expect(parseRoute(`/workspace/${WORKSPACE_ID}/`)).toEqual({
      name: "workspace",
      workspaceId: WORKSPACE_ID,
    });
    expect(
      parseRoute(`/workspace/${WORKSPACE_ID}/file/${DOCUMENT_ID}`),
    ).toEqual({
      name: "file",
      workspaceId: WORKSPACE_ID,
      documentId: DOCUMENT_ID,
    });
  });

  it("round-trips through formatRoute", () => {
    for (const path of [
      "/",
      "/people",
      "/invite",
      `/workspace/${WORKSPACE_ID}`,
      `/workspace/${WORKSPACE_ID}/file/${DOCUMENT_ID}`,
    ]) {
      expect(formatRoute(parseRoute(path))).toBe(path);
    }
  });

  it("sends anything malformed to not_found without reaching the API", () => {
    for (const path of [
      "/workspace",
      "/workspace/not-a-uuid",
      "/workspace/123",
      "/workspace/AAAAAAAA-AAAA-4AAA-8AAA-AAAAAAAAAAAA",
      `/workspace/${WORKSPACE_ID}/file`,
      `/workspace/${WORKSPACE_ID}/file/x`,
      `/workspace/${WORKSPACE_ID}/file/${DOCUMENT_ID}/extra`,
      `/workspace/${WORKSPACE_ID}/files/${DOCUMENT_ID}`,
      "/people/extra",
      "//",
      "/a//b",
      "/admin",
      "/workspace/../people",
    ]) {
      expect(parseRoute(path), path).toEqual({ name: "not_found" });
    }
  });
});

describe("return_to", () => {
  it("passes ordinary same-origin paths", () => {
    expect(safeReturnTo(`/workspace/${WORKSPACE_ID}/file/${DOCUMENT_ID}`)).toBe(
      `/workspace/${WORKSPACE_ID}/file/${DOCUMENT_ID}`,
    );
    expect(safeReturnTo("/people?tab=teams")).toBe("/people?tab=teams");
  });

  it("replaces every open-redirect shape and oversize values with /", () => {
    for (const bad of [
      "",
      "https://evil.example/",
      "//evil.example/",
      "/\\evil.example",
      "/a\\b",
      "javascript:alert(1)",
      "evil",
      "/with space",
      "/new\nline",
      "/café",
      "/" + "a".repeat(MAX_RETURN_TO),
    ]) {
      expect(safeReturnTo(bad), JSON.stringify(bad)).toBe("/");
    }
  });

  it("builds a sign-in URL that carries the encoded path only", () => {
    expect(signInHref(`/workspace/${WORKSPACE_ID}`)).toBe(
      `/api/v1/auth/login?return_to=%2Fworkspace%2F${WORKSPACE_ID}`,
    );
    expect(signInHref("//evil.example")).toBe(
      "/api/v1/auth/login?return_to=%2F",
    );
  });
});
