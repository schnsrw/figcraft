import { readFile } from "node:fs/promises";
import { fileURLToPath, pathToFileURL } from "node:url";

const ALLOWED_LICENSES = new Set([
  "0BSD",
  "Apache-2.0",
  "BSD-2-Clause",
  "BSD-3-Clause",
  "BlueOak-1.0.0",
  "CC0-1.0",
  "ISC",
  "MIT",
  "MIT-0",
  "Python-2.0",
  "Unicode-3.0",
  "Unlicense",
  "Zlib",
]);

const FORBIDDEN_LICENSE = /\b(?:AGPL|GPL|LGPL)-/iu;

function unwrapParentheses(expression) {
  let result = expression.trim();

  while (result.startsWith("(") && result.endsWith(")")) {
    result = result.slice(1, -1).trim();
  }

  return result;
}

export function isAllowedLicenseExpression(expression) {
  if (typeof expression !== "string" || expression.trim() === "") {
    return false;
  }

  const normalized = unwrapParentheses(expression);
  if (FORBIDDEN_LICENSE.test(normalized)) {
    return false;
  }

  const orTerms = normalized.split(/\s+OR\s+/u);
  if (orTerms.length > 1) {
    return orTerms.some(isAllowedLicenseExpression);
  }

  const andTerms = normalized.split(/\s+AND\s+/u);
  if (andTerms.length > 1) {
    return andTerms.every(isAllowedLicenseExpression);
  }

  return ALLOWED_LICENSES.has(normalized);
}

export function productionLicenseProblems(lockfile) {
  const problems = [];

  for (const [packagePath, metadata] of Object.entries(
    lockfile.packages ?? {},
  )) {
    if (
      !packagePath.startsWith("node_modules/") ||
      metadata.dev === true ||
      metadata.link === true
    ) {
      continue;
    }

    const license = metadata.license;
    if (!isAllowedLicenseExpression(license)) {
      const version =
        metadata.version === undefined ? "unknown version" : metadata.version;
      const reason =
        license === undefined
          ? "missing license metadata"
          : `license ${license}`;
      problems.push(`${packagePath} (${version}): ${reason}`);
    }
  }

  return problems;
}

async function main() {
  const lockfileUrl = new URL("../package-lock.json", import.meta.url);
  const lockfile = JSON.parse(await readFile(lockfileUrl, "utf8"));
  const problems = productionLicenseProblems(lockfile);

  if (problems.length > 0) {
    console.error("Production dependency license check failed:");
    for (const problem of problems) {
      console.error(`- ${problem}`);
    }
    process.exitCode = 1;
    return;
  }

  console.log("Production dependency licenses: ok");
}

const invokedPath = process.argv[1];
if (
  invokedPath !== undefined &&
  pathToFileURL(fileURLToPath(new URL(import.meta.url))).href ===
    pathToFileURL(invokedPath).href
) {
  await main();
}
