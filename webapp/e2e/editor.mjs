// S6-020: a file opens as the FigCraft editor itself (Rust/WASM, full page, no iframe).
//   E2E_BASE=http://127.0.0.1:8080 E2E_OWNER_PASSWORD_FILE=... E2E_SHOTS=./shots node editor.mjs
// Signs in, opens the first file of the first workspace from the file browser, waits for the
// editor's canvas, and screenshots the editor and its main menu. Our own UI only.
import { chromium } from "playwright-core";
import fs from "node:fs";

const base = process.env.E2E_BASE ?? "http://127.0.0.1:8080";
const out = process.env.E2E_SHOTS ?? "./shots";
const chrome =
  process.env.E2E_CHROME ??
  "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const login = process.env.E2E_OWNER_LOGIN ?? "owner";
// A plain password, or a "Password: …" line in the file.
const secret = fs.readFileSync(process.env.E2E_OWNER_PASSWORD_FILE, "utf8");
const password =
  process.env.E2E_OWNER_PASSWORD ??
  (secret.match(/^Password:\s*(.+)$/m)?.[1] ?? secret).trim();
fs.mkdirSync(out, { recursive: true });

// Headless Chrome has no GPU: SwiftShader gives the editor WebGL2.
const browser = await chromium.launch({
  executablePath: chrome,
  headless: true,
  args: ["--use-angle=swiftshader", "--enable-unsafe-swiftshader", "--ignore-gpu-blocklist"],
});
const page = await (
  await browser.newContext({ viewport: { width: 1440, height: 900 } })
).newPage();
const problems = [];
page.on("pageerror", (e) => problems.push(`page error ${String(e).slice(0, 200)}`));
page.on("response", (r) => {
  if (r.status() >= 400) problems.push(`${r.status()} ${r.url().replace(base, "")}`);
});
page.on("console", (m) => {
  if (m.type() === "error") problems.push(`console ${m.text().slice(0, 200)}`);
});

await page.goto(`${base}/`);
await page.getByTestId("signin-login").fill(login);
await page.getByTestId("signin-password").fill(password);
await page.getByTestId("signin-submit").click();
await page.getByRole("heading", { name: "Your files" }).waitFor({ timeout: 15000 });

// The first workspace, then its first file (adding one if the project has none).
await page.locator('a[href^="/workspace/"]').first().click();
await page.getByRole("list", { name: "Files" }).or(page.getByText(/No files/)).first().waitFor({ timeout: 15000 });
let file = page.getByRole("list", { name: "Files" }).getByRole("link").first();
if ((await file.count()) === 0) {
  await page.getByRole("button", { name: "New file" }).click();
  await page.getByLabel("New file").fill("Editor check");
  await page.getByRole("button", { name: "Add file" }).click();
} else {
  await file.click();
}
await page.waitForURL(/\/workspace\/[0-9a-f-]{36}\/file\/[0-9a-f-]{36}$/, { timeout: 15000 });
const url = page.url();
// WebGL keeps headless Chrome on one backend.
await page.goto(`${url}?webgl`);
await page.locator("#figcraft_canvas").waitFor({ timeout: 30000 });
const started = await page
  .waitForFunction(() => !document.getElementById("figcraft_loading"), null, { timeout: 60000 })
  .then(() => true, () => false);
if (!started) {
  await page.screenshot({ path: `${out}/editor-stuck.png` });
  const loading = await page.locator("#figcraft_loading").textContent().catch(() => null);
  console.log(JSON.stringify({ url: page.url(), loading, problems }, null, 2));
  await browser.close();
  process.exit(1);
}
await page.waitForTimeout(6000);

const frames = await page.locator("iframe").count();
const box = await page.locator("#figcraft_canvas").boundingBox();
const title = await page.title();
await page.screenshot({ path: `${out}/editor.png` });
// The main menu (the rail's top button).
await page.mouse.click(28, 24);
await page.waitForTimeout(800);
await page.screenshot({ path: `${out}/editor-menu.png` });

// File ▸ (local Open/Save/Close are hidden for a cloud file).
await page.mouse.move(60, 145);
await page.waitForTimeout(800);
await page.screenshot({ path: `${out}/editor-file-menu.png` });
// Back to files: the first row of the main menu.
await page.mouse.move(60, 71);
await page.mouse.click(60, 71);
await page.waitForURL(/\/workspace\/[0-9a-f-]{36}$/, { timeout: 15000 });
const back = new URL(page.url()).pathname;

console.log(JSON.stringify({ url, frames, canvas: box, title, back, problems }, null, 2));
await browser.close();
if (frames !== 0 || !box || box.width < 1400) process.exit(1);
