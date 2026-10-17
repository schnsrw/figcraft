// TS-003/TS-004: the whole team path in a real browser. See README.md.
import { chromium } from "playwright-core";
import fs from "node:fs";
import crypto from "node:crypto";

const base = process.env.E2E_BASE ?? "http://127.0.0.1:8080";
const shots = process.env.E2E_SHOTS ?? "./shots";
const chrome =
  process.env.E2E_CHROME ??
  "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
const login = process.env.E2E_OWNER_LOGIN ?? "owner";
const password =
  process.env.E2E_OWNER_PASSWORD ??
  `Ts!${crypto.randomBytes(12).toString("hex")}Zq9`;
const token = process.env.E2E_SETUP_TOKEN;
const stamp = new Date().toISOString().slice(11, 19).replaceAll(":", "");
const workspace = `E2E ${stamp}`;
fs.mkdirSync(shots, { recursive: true });

const browser = await chromium.launch({
  executablePath: chrome,
  headless: true,
  args: [
    "--use-angle=swiftshader",
    "--enable-unsafe-swiftshader",
    "--ignore-gpu-blocklist",
    "--enable-webgl",
  ],
});
const problems = [];
const step = (name) => console.log(`- ${name}`);
const must = (ok, what) => {
  if (!ok) {
    problems.push(what);
    console.log(`  FAIL: ${what}`);
  }
};

async function session(name) {
  const page = await (
    await browser.newContext({ viewport: { width: 1280, height: 800 } })
  ).newPage();
  page.on("pageerror", (e) =>
    problems.push(`${name}: page error ${String(e).slice(0, 160)}`),
  );
  page.on("response", (r) => {
    if (r.status() >= 500)
      problems.push(`${name}: HTTP ${r.status()} ${r.url().replace(base, "")}`);
  });
  return {
    page,
    shot: (n) => page.screenshot({ path: `${shots}/${name}-${n}.png` }),
  };
}

async function signIn(page) {
  await page.goto(`${base}/`);
  await page.getByTestId("signin-login").fill(login);
  await page.getByTestId("signin-password").fill(password);
  await page.getByTestId("signin-submit").click();
  await page.getByText("Your files").waitFor({ timeout: 15000 });
}

async function openEditor(page) {
  await page.getByRole("link", { name: workspace }).first().click();
  await page.getByText("Mobile app").first().click();
  await page.getByText("Home screen").first().click();
  await page.getByRole("button", { name: "Open in editor" }).click();
  await page.waitForURL(/\/edit\//, { timeout: 15000 });
  await page.waitForTimeout(15000);
}

const A = await session("A");
step("first run or sign-in");
if (token) {
  await A.page.goto(`${base}/setup`);
  await A.page.getByTestId("setup-token").fill(token);
  await A.page.getByTestId("setup-name").fill("Test Owner");
  await A.page.getByTestId("setup-login").fill(login);
  await A.page.getByTestId("setup-password").fill(password);
  await A.page.getByTestId("setup-confirm").fill(password);
  await A.page.getByTestId("setup-submit").click();
  console.log(
    `  owner "${login}" created with password ${process.env.E2E_OWNER_PASSWORD ? "(given)" : password}`,
  );
} else {
  await signIn(A.page);
}
await A.page.getByText("Your files").waitFor({ timeout: 15000 });
await A.shot("01-home");

step("workspace, project, file");
await A.page.getByLabel("Workspace name").fill(workspace);
await A.page.getByRole("button", { name: "Create workspace" }).click();
await A.page.getByRole("link", { name: workspace }).first().click();
await A.page
  .getByLabel(/New project/i)
  .first()
  .fill("Mobile app");
await A.page.getByRole("button", { name: "Add project" }).click();
await A.page.getByText("Mobile app").first().click();
await A.page
  .getByLabel(/New file/i)
  .first()
  .fill("Home screen");
await A.page.getByRole("button", { name: "Add file" }).click();
await A.page.getByText("Home screen").first().waitFor({ timeout: 10000 });
await A.shot("02-project");

step("two sessions open the file");
await A.page.getByText("Home screen").first().click();
await A.page.getByRole("button", { name: "Open in editor" }).click();
await A.page.waitForURL(/\/edit\//, { timeout: 15000 });
await A.page.waitForTimeout(15000);
const B = await session("B");
await signIn(B.page);
await openEditor(B.page);
await A.shot("03-editor");

step("A draws a rectangle, B draws an ellipse");
const draw = async (s, key, from, to) => {
  await s.page.mouse.move(...from);
  await s.page.keyboard.press(key);
  await s.page.mouse.move(...from);
  await s.page.mouse.down();
  await s.page.mouse.move(...to, { steps: 8 });
  await s.page.mouse.up();
};
await draw(A, "r", [500, 300], [640, 400]);
await B.page.waitForTimeout(4000);
await draw(B, "o", [700, 450], [820, 540]);
await A.page.waitForTimeout(5000);
await A.shot("04-A-after-both");
await B.shot("05-B-after-both");

step("a third session opens the file later and sees both shapes");
const C = await session("C");
await signIn(C.page);
await openEditor(C.page);
await C.shot("06-late-joiner");
console.log(
  `screenshots in ${shots}; compare 04, 05 and 06 by eye or with an image diff`,
);
await browser.close();
if (problems.length) {
  console.log("PROBLEMS:\n" + problems.join("\n"));
  process.exit(1);
}
console.log("run finished without errors");
