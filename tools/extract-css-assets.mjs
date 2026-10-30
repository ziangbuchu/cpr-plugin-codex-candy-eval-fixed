// 把样式表里的 data: URL 抽成独立的注册资源文件。
//
// CPR 插件页面运行在 sandbox iframe 中，装配阶段会拒绝一切内联 data: 地址，
// 因此样式表里的字体与背景图必须落成真实文件，并在 plugin.json 中登记 MIME。
//
// 用法：node tools/extract-css-assets.mjs <css 文件> <资源目录> <文件名前缀>

import { createHash } from "node:crypto";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import path from "node:path";
import { parse, walk } from "css-tree";

const [cssFile, assetDir, prefix] = process.argv.slice(2);
if (!cssFile || !assetDir || !prefix) {
  console.error("用法：node tools/extract-css-assets.mjs <css 文件> <资源目录> <前缀>");
  process.exit(2);
}

const EXTENSIONS = { "font/woff2": "woff2", "font/woff": "woff", "image/svg+xml": "svg", "image/png": "png" };
mkdirSync(assetDir, { recursive: true });

let source = readFileSync(cssFile, "utf8");
const extracted = [];

for (;;) {
  const tree = parse(source, { context: "stylesheet", parseRulePrelude: false, parseCustomProperty: true, positions: true });
  const targets = [];
  walk(tree, (node) => {
    if ((node.type === "Url" || node.type === "String") && node.value.startsWith("data:")) {
      targets.push({ node, loc: node.loc, value: node.value });
    }
  });
  if (!targets.length) break;

  const { node, loc, value } = targets[0];
  const header = value.slice("data:".length).split(",", 1)[0];
  const payload = value.slice("data:".length + header.length + 1);
  const [mime, ...flags] = header.split(";");
  const data = flags.includes("base64")
    ? Buffer.from(payload, "base64")
    : Buffer.from(decodeURIComponent(payload), "utf8");
  const extension = EXTENSIONS[mime];
  if (!extension) throw new Error(`未知资源类型：${mime}`);
  const name = `${prefix}-${createHash("sha256").update(data).digest("hex").slice(0, 12)}.${extension}`;
  writeFileSync(path.join(assetDir, name), data);
  extracted.push({ name, mime, bytes: data.length });

  // css-tree 的偏移量是 UTF-8 字节位置，且 Url 节点覆盖整个 url(...) token，
  // 因此按字节整体替换为 url(相对文件名)。
  const reference = path.relative(path.dirname(path.resolve(cssFile)), path.resolve(assetDir, name)).split(path.sep).join("/");
  const bytes = Buffer.from(source, "utf8");
  const replacement = node.type === "Url" ? `url(${reference})` : `"${reference}"`;
  source = bytes.subarray(0, loc.start.offset).toString("utf8") + replacement + bytes.subarray(loc.end.offset).toString("utf8");
}

writeFileSync(cssFile, source);
console.log(`${cssFile}: ${JSON.stringify(extracted, null, 0)}`);
