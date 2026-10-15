import { describe, expect, it } from "vitest";

import { EDITOR_BRIDGE_PROTOCOL_VERSION } from "./index";

describe("editor bridge protocol", () => {
  it("exposes the supported major version at runtime", () => {
    expect(EDITOR_BRIDGE_PROTOCOL_VERSION).toBe(1);
  });
});
