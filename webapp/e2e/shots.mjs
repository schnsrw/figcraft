// TS-015: screenshots of every workspace screen, for design review and before/after docs.
//   E2E_BASE=http://127.0.0.1:8080 E2E_OWNER_PASSWORD_FILE=... E2E_SHOTS=./shots node shots.mjs
// Screens are captured at desktop and tablet width, in light and dark. Our own UI only.
import { chromium } from "playwright-core";
import fs from "node:fs";

const base = process.env.E2E_BASE ?? "http://127.0.0.1:8080";
const out = process.env.E2E_SHOTS ?? "./shots";
const chrome =
  process.env.E2E_CHROME ??
  "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const login = process.env.E2E_OWNER_LOGIN ?? "owner";
const password =
  process.env.E2E_OWNER_PASSWORD ??
  fs.readFileSync(process.env.E2E_OWNER_PASSWORD_FILE, "utf8").trim();
const only = (process.env.E2E_ONLY ?? "").split(",").filter(Boolean);
const schemes = (process.env.E2E_SCHEMES ?? "dark,light").split(",");
const widths = (process.env.E2E_WIDTHS ?? "1280,820").split(",").map(Number);
fs.mkdirSync(out, { recursive: true });

const browser = await chromium.launch({
  executablePath: chrome,
  headless: true,
});
const problems = [];

async function context(scheme, width) {
  const ctx = await browser.newContext({
    viewport: { width, height: width >= 1000 ? 800 : 1100 },
    colorScheme: scheme,
  });
  const page = await ctx.newPage();
  page.on("pageerror", (e) =>
    problems.push(`page error ${String(e).slice(0, 200)}`),
  );
  return page;
}

// The stack rate-limits bursts; pace navigation and wait out "Too many requests".
async function go(page, url) {
  for (let attempt = 0; attempt < 8; attempt++) {
    await page.waitForTimeout(2500);
    await page.goto(url);
    await page.waitForLoadState("networkidle").catch(() => {});
    const limited = (await page.getByText("Too many requests").count()) > 0;
    if (!limited) return;
    console.log("rate limited; waiting 20s");
    await page.waitForTimeout(20000);
  }
}

async function signIn(page) {
  await go(page, `${base}/`);
  await page.getByTestId("signin-login").fill(login);
  await page.getByTestId("signin-password").fill(password);
  await page.getByTestId("signin-submit").click();
  await page
    .getByRole("heading", { name: "Your files" })
    .waitFor({ timeout: 15000 });
}

async function settle(page) {
  await page.waitForLoadState("networkidle").catch(() => {});
  await page.waitForTimeout(400);
}

async function firstHref(page, pattern) {
  return page.evaluate((source) => {
    const re = new RegExp(source);
    const a = Array.from(document.querySelectorAll("a")).find((x) =>
      re.test(x.getAttribute("href") ?? ""),
    );
    return a ? a.getAttribute("href") : null;
  }, pattern.source);
}

for (const scheme of schemes) {
  for (const width of widths) {
    const tag = `${scheme}-${width}`;
    const shot = async (page, name) => {
      if (only.length && !only.includes(name)) return;
      await settle(page);
      await page.screenshot({
        path: `${out}/${name}-${tag}.png`,
        fullPage: true,
      });
      console.log(`shot ${name}-${tag}`);
    };

    // Signed-out screens.
    const anon = await context(scheme, width);
    await go(anon, `${base}/`);
    await anon.getByTestId("signin-login").waitFor();
    await shot(anon, "signin");
    await anon.getByTestId("signin-login").fill("owner");
    await anon.getByTestId("signin-password").fill("wrong-password");
    await anon.getByTestId("signin-submit").click();
    await anon.getByTestId("signin-error").waitFor();
    await shot(anon, "signin-error");
    await go(anon, `${base}/setup`);
    await anon.getByTestId("setup-token").waitFor();
    await shot(anon, "setup");
    await anon.close();

    // Signed in.
    const page = await context(scheme, width);
    await signIn(page);
    await shot(page, "home");
    const workspaceHref = await firstHref(page, /^\/workspace\/[0-9a-f-]{36}$/);
    if (workspaceHref) {
      await go(page, `${base}${workspaceHref}`);
      await page.getByRole("heading", { level: 1 }).waitFor();
      await shot(page, "workspace");
      const fileHref = await firstHref(page, /\/file\//);
      if (fileHref) {
        await go(page, `${base}${fileHref}`);
        await page
          .getByRole("button", { name: "Open in editor" })
          .waitFor({ timeout: 10000 })
          .catch(async (error) => {
            await page.screenshot({
              path: `${out}/debug-file-${tag}.png`,
              fullPage: true,
            });
            console.log(
              fileHref,
              (await page.locator("body").innerText()).slice(0, 400),
            );
            throw error;
          });
        await shot(page, "file");
        const share = page.getByRole("button", { name: "Share" });
        if (await share.count()) {
          await share.click();
          await page.getByRole("dialog").waitFor();
          await shot(page, "share");
        }
      }
    }
    for (let attempt = 0; attempt < 6; attempt++) {
      await go(page, `${base}/people`);
      const limited = await page
        .getByText("Too many requests")
        .waitFor({ timeout: 3000 })
        .then(() => true)
        .catch(() => false);
      if (!limited) break;
      console.log("rate limited; waiting 20s");
      await page.waitForTimeout(20000);
    }
    await page
      .getByRole("tab", { name: "Members" })
      .waitFor({ timeout: 10000 })
      .catch(async (error) => {
        await page.screenshot({
          path: `${out}/debug-people-${tag}.png`,
          fullPage: true,
        });
        console.log((await page.locator("body").innerText()).slice(0, 400));
        throw error;
      });
    await shot(page, "people-members");
    const add = page.getByTestId("addperson-open");
    if (await add.count()) {
      await add.click();
      await shot(page, "people-add");
    }
    await page.getByRole("tab", { name: "Teams" }).click();
    await shot(page, "people-teams");
    await page.getByRole("tab", { name: "Invitations" }).click();
    await shot(page, "people-invitations");
    // Pages an older deployment may not have yet are skipped rather than failing the run.
    for (const [path, heading, name] of [
      ["/account", "Your account", "account"],
      ["/settings", "Settings", "settings"],
      ["/search?q=a", "Search", "search"],
    ]) {
      await go(page, `${base}${path}`);
      const found = await page
        .getByRole("heading", { name: heading })
        .waitFor({ timeout: 4000 })
        .then(() => true)
        .catch(() => false);
      if (found) await shot(page, name);
      else console.log(`skip ${name} (not in this build)`);
    }
    await go(page, `${base}/nope`);
    await shot(page, "not-found");
    await page.close();
  }
}
await browser.close();
if (problems.length) {
  console.log("PROBLEMS:\n" + problems.join("\n"));
  process.exitCode = 1;
}
