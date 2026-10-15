import { createHash } from "node:crypto";
import { readdir, readFile, stat } from "node:fs/promises";
import { dirname, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { spawn } from "node:child_process";

const IGNORED_DIRECTORIES = new Set(["coverage", "dist", "node_modules"]);

async function isDirectory(path) {
  try {
    return (await stat(path)).isDirectory();
  } catch (error) {
    if (error?.code === "ENOENT") {
      return false;
    }
    throw error;
  }
}

async function snapshotFiles(root, current = root, result = new Map()) {
  const entries = await readdir(current, { withFileTypes: true });

  for (const entry of entries) {
    if (entry.isDirectory() && IGNORED_DIRECTORIES.has(entry.name)) {
      continue;
    }

    const path = resolve(current, entry.name);
    if (entry.isDirectory()) {
      await snapshotFiles(root, path, result);
    } else if (entry.isFile()) {
      const digest = createHash("sha256")
        .update(await readFile(path))
        .digest("hex");
      result.set(relative(root, path), digest);
    }
  }

  return result;
}

function changedPaths(before, after) {
  const paths = new Set([...before.keys(), ...after.keys()]);
  return [...paths]
    .filter((path) => before.get(path) !== after.get(path))
    .sort();
}

function runGenerator(webappRoot, packageName) {
  return new Promise((resolvePromise, reject) => {
    const child = spawn(
      "npm",
      ["run", "generate", `--workspace=${packageName}`],
      { cwd: webappRoot, stdio: "inherit" },
    );

    child.once("error", reject);
    child.once("exit", (code, signal) => {
      if (code === 0) {
        resolvePromise();
      } else {
        reject(
          new Error(
            `API client generator failed (${signal === null ? `exit ${code}` : signal})`,
          ),
        );
      }
    });
  });
}

async function main() {
  const webappRoot = dirname(dirname(fileURLToPath(import.meta.url)));
  const clientRoot = resolve(webappRoot, "packages/api-client");

  if (!(await isDirectory(clientRoot))) {
    console.log("Generated API client: not present yet; no output to verify");
    return;
  }

  const packageManifest = JSON.parse(
    await readFile(resolve(clientRoot, "package.json"), "utf8"),
  );
  if (
    typeof packageManifest.name !== "string" ||
    typeof packageManifest.scripts?.generate !== "string"
  ) {
    throw new Error(
      "The API client package must have a name and deterministic generate script",
    );
  }

  const before = await snapshotFiles(clientRoot);
  await runGenerator(webappRoot, packageManifest.name);
  const changed = changedPaths(before, await snapshotFiles(clientRoot));

  if (changed.length > 0) {
    throw new Error(
      `Generated API client is stale; regenerate and commit: ${changed.join(", ")}`,
    );
  }

  console.log("Generated API client: up to date");
}

try {
  await main();
} catch (error) {
  console.error(error instanceof Error ? error.message : error);
  process.exitCode = 1;
}
