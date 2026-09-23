// Browser smoke test: boots the app and clicks through every tab, failing on
// any uncaught error. Catches what the bundler can't — module-eval ordering
// (the Buffer polyfill) and render-time crashes on real indexed data.
//
// Needs the validator, the backend and `npm run dev` up. Run: node smoke.mjs
import { chromium } from "playwright";

const URL = process.argv[2] || "http://localhost:5173/?cluster=localnet";
const EXE = process.env.PLAYWRIGHT_CHROMIUM; // set to reuse a cached build

const b = await chromium.launch(EXE ? { executablePath: EXE } : {});
const p = await b.newPage();
const errors = [];
p.on("pageerror", e => errors.push(String(e).split("\n")[0]));
p.on("console", m => m.type() === "error" && errors.push("console: " + m.text().slice(0, 160)));

await p.goto(URL, { waitUntil: "networkidle", timeout: 30000 });
await p.waitForTimeout(2000);

const buffer = await p.evaluate(() => typeof globalThis.Buffer);
if (buffer !== "function") errors.push("Buffer polyfill did not run (typeof = " + buffer + ")");

for (const tab of ["Dashboard", "Trade", "Liquidity", "Positions", "History", "Status"]) {
  await p.getByRole("button", { name: tab, exact: true }).click();
  await p.waitForTimeout(900);
  const body = (await p.evaluate(() => document.body.innerText)).replace(/\s+/g, " ");
  if (body.length < 80) errors.push(`${tab}: rendered nothing`);
  console.log(`${tab.padEnd(10)} ${body.slice(body.indexOf("Status") + 7, body.indexOf("Status") + 130).trim()}`);
}

await b.close();
if (errors.length) { console.error("\nFAIL:\n  " + errors.join("\n  ")); process.exit(1); }
console.log("\nok — all tabs rendered, no uncaught errors");
