import { describe, expect, it } from "vitest";

import {
  isAllowedLicenseExpression,
  productionLicenseProblems,
} from "./check-licenses.mjs";

describe("production dependency license policy", () => {
  it("accepts permissive SPDX expressions", () => {
    expect(isAllowedLicenseExpression("MIT")).toBe(true);
    expect(isAllowedLicenseExpression("(MIT OR Apache-2.0)")).toBe(true);
    expect(isAllowedLicenseExpression("MIT AND BSD-3-Clause")).toBe(true);
  });

  it("rejects forbidden, unknown and missing licenses", () => {
    expect(isAllowedLicenseExpression("GPL-3.0-only OR MIT")).toBe(false);
    expect(isAllowedLicenseExpression("MPL-2.0")).toBe(false);
    expect(isAllowedLicenseExpression(undefined)).toBe(false);
  });

  it("checks production packages and ignores development-only and workspace links", () => {
    const problems = productionLicenseProblems({
      packages: {
        "node_modules/allowed": { license: "MIT", version: "1.0.0" },
        "node_modules/development-only": {
          dev: true,
          license: "GPL-3.0-only",
          version: "1.0.0",
        },
        "node_modules/local-workspace": { link: true },
        "node_modules/missing": { version: "2.0.0" },
      },
    });

    expect(problems).toEqual([
      "node_modules/missing (2.0.0): missing license metadata",
    ]);
  });
});
