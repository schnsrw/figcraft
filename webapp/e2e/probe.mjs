// Debug helper: open one path signed in and save a screenshot plus the visible text.
//   E2E_BASE=… E2E_OWNER_PASSWORD_FILE=… node probe.mjs /people out.png
import { chromium } from "playwright-core";
import fs from "node:fs";

const base = process.env.E2E_BASE ?? "http://127.0.0.1:8080";
const chrome =
  process.env.E2E_CHROME ??
  "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const password =
  process.env.E2E_OWNER_PASSWORD ??
  fs.readFileSync(process.env.E2E_OWNER_PASSWORD_FILE, "utf8").trim();
const [path = "/", out = "probe.png"] = process.argv.slice(2);

const browser = await chromium.launch({
  executablePath: chrome,
  headless: true,
});
const page = await (
  await browser.newContext({ viewport: { width: 1280, height: 800 } })
).newPage();
page.on("pageerror", (e) => console.log("pageerror", String(e).slice(0, 300)));
page.on(
  "console",
  (m) => m.type() === "error" && console.log("console", m.text().slice(0, 300)),
);
await page.goto(`${base}/`);
await page
  .getByTestId("signin-login")
  .fill(process.env.E2E_OWNER_LOGIN ?? "owner");
await page.getByTestId("signin-password").fill(password);
await page.getByTestId("signin-submit").click();
await page.getByRole("heading", { name: "Your files" }).waitFor();
await page.goto(`${base}${path}`);
await page.waitForTimeout(2500);
await page.screenshot({ path: out, fullPage: true });
console.log((await page.locator("body").innerText()).slice(0, 600));
await browser.close();
