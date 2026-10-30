"use strict";
// 校验 ui.html 的 CPR 宿主资源装配约束；在 jsdom 中以内联脚本和模拟桥验证渲染与交互。
// 这是本地兼容性检查，不替代真实 CPR iframe/CSP/桥运行时的浏览器验收。
//
// 用法：node tests/check-page.mjs <plugin-dir>

import { readFileSync } from "node:fs";
import path from "node:path";
import { JSDOM } from "jsdom";
import * as csstree from "css-tree";
import { buildPage } from "../tools/build-page.mjs";

const root = path.resolve(process.argv[2] || ".");
const manifest = JSON.parse(readFileSync(path.join(root, "plugin.json"), "utf8"));
const resources = new Map();
for (const [rel, mime] of Object.entries(manifest.resources)) {
  resources.set(rel, { mime, body: readFileSync(path.join(root, rel)) });
}

const failures = [];
const fail = (message) => failures.push(message);
const check = (condition, message) => { if (!condition) fail(message); };

const PATH_SEGMENT = /^(?!\.{1,2}$)[\w.-]+$/u;
const JAVASCRIPT = new Set(["application/ecmascript", "application/javascript", "text/ecmascript", "text/javascript"]);
const mime = (value) => String(value).split(";")[0].trim().toLowerCase();

function resolveReference(ownerPath, raw) {
  const reference = String(raw).trim();
  if (!reference || reference.startsWith("/") || reference.startsWith("//") || reference.includes("\\")
    || reference.includes("%") || reference.includes("?") || reference.includes("#")
    || /^[a-z][a-z\d+.-]*:/iu.test(reference)) {
    throw new Error(`资源地址不受支持：${raw}`);
  }
  const base = ownerPath.split("/").slice(0, -1);
  for (const segment of reference.split("/")) {
    if (!segment || segment === ".") continue;
    if (segment === "..") { if (!base.length) throw new Error(`越过制品边界：${raw}`); base.pop(); continue; }
    if (!PATH_SEGMENT.test(segment)) throw new Error(`路径片段无效：${raw}`);
    base.push(segment);
  }
  const resolved = base.join("/");
  if (!resources.has(resolved)) throw new Error(`引用了未注册资源：${resolved}`);
  return resolved;
}

const dataUrl = (body, contentType) => `data:${contentType};base64,${body.toString("base64")}`;

function rewriteCss(source, ownerPath, allowImports = true) {
  const tree = csstree.parse(source, {
    context: allowImports ? "stylesheet" : "declarationList",
    parseRulePrelude: false,
    parseCustomProperty: true,
    onParseError(_error, fallback) { if (fallback) throw new Error(`CSS 语法不受支持：${ownerPath}`); },
  });
  const imported = new WeakSet();
  csstree.walk(tree, function (node) {
    if (node.type === "Raw" && node !== this.rule?.prelude) throw new Error(`CSS 含无法解析内容：${ownerPath}`);
    if ((node.type === "Atrule" || node.type === "Function") && node.name.includes("\\")) throw new Error(`CSS 规则名转义：${ownerPath}`);
    if (node.type === "Atrule" && node.name.toLowerCase() === "import") {
      if (!allowImports) throw new Error("内联 style 不能 @import");
      const reference = node.prelude?.type === "AtrulePrelude" ? node.prelude.children.first : null;
      if (node.block || !reference || (reference.type !== "String" && reference.type !== "Url")) throw new Error(`@import 语法不受支持：${ownerPath}`);
      const resolved = resolveReference(ownerPath, reference.value);
      if (mime(resources.get(resolved).mime) !== "text/css") throw new Error(`@import 目标不是 CSS：${resolved}`);
      reference.value = dataUrl(resources.get(resolved).body, "text/css");
      imported.add(reference);
    }
    if (node.type === "Url" && !imported.has(node)) {
      if (node.value.startsWith("#")) return;
      const resolved = resolveReference(ownerPath, node.value);
      node.value = dataUrl(resources.get(resolved).body, resources.get(resolved).mime);
    }
  });
  return csstree.generate(tree);
}

// ---------------------------------------------------------------- 装配校验

const entry = "web/ui.html";
const entryBody = buildPage(root);
resources.set(entry, { mime: "text/html", body: Buffer.from(entryBody) });
const document = new JSDOM(entryBody).window.document;
check(document.querySelector("footer")?.textContent.includes(`版本 v${manifest.version}`), "页脚必须显示实际插件版本，不能保留上游 v1.0.0");
check(![...resources.keys()].some((resource) => resource.endsWith(".js")), "发布清单不能预取独立脚本资源");
check(!document.querySelector("script[src]"), "发布入口必须内联全部脚本");
check(Buffer.byteLength(entryBody) <= 1024 * 1024, "发布入口超过 CPR 1 MiB 限制");

const unsupported = document.querySelector("base, iframe, frame, frameset, object, embed, portal, template");
check(!unsupported, `页面使用了不受支持的元素：${unsupported?.tagName}`);
check(![...document.querySelectorAll("meta[http-equiv]")].some((meta) => meta.getAttribute("http-equiv")?.toLowerCase() === "refresh"), "页面不能自动跳转");

for (const script of document.querySelectorAll("script")) {
  check(script.getAttribute("type")?.trim().toLowerCase() !== "module", "页面不能使用 ES Module");
  const source = script.getAttribute("src");
  if (!source) continue;
  try {
    const resolved = resolveReference(entry, source);
    check(JAVASCRIPT.has(mime(resources.get(resolved).mime)), `脚本资源类型无效：${resolved}`);
  } catch (error) { fail(error.message); }
}

for (const link of document.querySelectorAll("link[href]")) {
  const rel = new Set((link.getAttribute("rel") ?? "").toLowerCase().split(/\s+/u).filter(Boolean));
  check([...rel].every((value) => value === "stylesheet" || value === "icon") && rel.size > 0, `只支持 stylesheet/icon 链接：${link.outerHTML}`);
  try {
    const resolved = resolveReference(entry, link.getAttribute("href"));
    const resolvedMime = mime(resources.get(resolved).mime);
    if (rel.has("stylesheet")) check(resolvedMime === "text/css", `样式资源类型无效：${resolved}`);
    if (resolvedMime === "text/css") rewriteCss(resources.get(resolved).body.toString("utf8"), resolved);
  } catch (error) { fail(error.message); }
}

const supportedSource = new Set(["AUDIO", "IMG", "SCRIPT", "SOURCE", "TRACK", "VIDEO"]);
for (const element of document.querySelectorAll("[src]")) {
  if (element.tagName === "SCRIPT") continue;
  check(supportedSource.has(element.tagName), `不支持 <${element.tagName.toLowerCase()} src>`);
  try { resolveReference(entry, element.getAttribute("src")); } catch (error) { fail(error.message); }
}

for (const element of document.querySelectorAll("*")) {
  for (const attribute of [...element.attributes]) {
    const name = attribute.name.toLowerCase();
    if (name === "href" || name === "xlink:href") {
      if (element.tagName === "LINK" || attribute.value.startsWith("#")) continue;
      fail(`页面不能导航到外部地址：<${element.tagName.toLowerCase()} href="${attribute.value}">`);
    }
    if (["action", "formaction", "ping", "background"].includes(name) && attribute.value) fail(`不支持 ${name} 地址`);
    if (name !== "style" && /\burl\s*\(/iu.test(attribute.value) && !/^url\(\s*#[\w.-]+\s*\)$/iu.test(attribute.value.trim())) {
      fail(`属性不能引用外部资源：${name}`);
    }
  }
}
for (const style of document.querySelectorAll("style")) {
  try { rewriteCss(style.textContent ?? "", entry, false); } catch (error) { fail(error.message); }
}

// ---------------------------------------------------------------- 沙箱渲染

const GLOBALS_BLOCKED = ["fetch", "XMLHttpRequest", "WebSocket", "localStorage", "sessionStorage"];

const calls = [];
let mockAnswers = [true, false, true];
// /run 的真实 CPR 契约：started 为整数，另外三项为账号 ID 数组。
let mockRunResponse = { started: 1, skipped: [], unchecked: [], busy: [] };
let mockRunGate = null, mockRunStatus = 200;
let mockRunning = {};
let mockHourly = { enabled: true, interval_seconds: 3600, model: "gpt-6.1-sol", effort: "low", next_run_at: new Date(Date.now() + 3600000).toISOString() };

function mockBackend(call) {
  // 真实插件状态里失败记录的字段集：没有 attribution，samples 为空数组。
  const mtRecord = {
    id: "mt-1", bank_revision: "rev1", status: "error", model: "gpt-6.1-sol",
    time: new Date(Date.now() - 60000).toISOString(), duration_ms: 7680, concurrency: 3,
    input_tokens: 0, output_tokens: 0, reasoning_tokens: 0, samples: [],
    error: "模型请求失败：no upstream provider is currently available for this request",
  };
  const credential = {
    id: "acct-1", provider: "codex", source: "cpr_accounts", name: "主账号",
    provider_name: "codex", email: "a@example.invalid", plan_type: "",
    disabled: false, unavailable: false, status_message: "",
    running: null, results: [], pelicans: [], fingerprints: [], modeltraces: [mtRecord],
  };
  const routes = {
    "GET api/accounts": () => ({ accounts: [credential], revision: "rev1" }),
    "GET api/models": () => ({ data: [{ id: "gpt-6.1-sol" }, { id: "gpt-6.1-sol(xhigh)" }] }),
    "GET api/accounts/models": () => ({ models: [{ id: "gpt-6.1-sol" }] }),
    "POST api/run": () => mockRunResponse,
    "POST api/modeltrace/run": () => mockRunResponse,
    "POST api/pelican/run": () => mockRunResponse,
    "POST api/fingerprint/run": () => mockRunResponse,
    "POST api/cancel": () => { mockRunning = {}; return { cancelled: true }; },
    "POST api/pelican/cancel": () => { mockRunning = {}; return { cancelled: true }; },
    "POST api/fingerprint/cancel": () => { mockRunning = {}; return { status: "cancelling" }; },
    "POST api/modeltrace/cancel": () => { mockRunning = {}; return { status: "cancelling" }; },
    "GET api/modeltrace/record": () => mtRecord,
  };
  if (`${call.method} ${call.path}` === "GET api/state") {
    const results = mockAnswers.map((ok, index) => ({
      skipped: false, time: new Date(Date.now() - index * 1000).toISOString(), model: "gpt-6.1-sol",
      effort: "low", ok, answer: ok ? "21" : "22", error: "", input_tokens: 10,
      output_tokens: 5, reasoning_tokens: 0, duration_ms: 1200,
    }));
    return { status: 200, body: { auths: [{ ...credential, results, ...mockRunning }], models: ["gpt-6.1-sol"], hourly_candy: mockHourly, storage_error: "", version: "1.0.0" } };
  }
  const handler = routes[`${call.method} ${call.path}`];
  if (!handler) return { status: 404, body: { error: { message: `未注册路由：${call.method} ${call.path}` } } };
  const isRun = call.method === "POST" && call.path.endsWith("/run");
  const respond = () => ({ status: isRun ? mockRunStatus : 200, body: handler() });
  return isRun && mockRunGate ? mockRunGate.then(respond) : respond();
}

// 复刻 frame.ts 的装配结果：脚本与样式内联成页面自带内容，桥脚本前置注入 <head>。
// 宿主用 data: URL 承载子资源；jsdom 不会加载 data: 脚本，这里改为等价的内联脚本。
const rewritten = new JSDOM(entryBody);
for (const script of [...rewritten.window.document.querySelectorAll("script[src]")]) {
  const resolved = resolveReference(entry, script.getAttribute("src"));
  script.removeAttribute("src");
  script.textContent = resources.get(resolved).body.toString("utf8");
}
for (const link of [...rewritten.window.document.querySelectorAll("link[rel~=stylesheet]")]) {
  const resolved = resolveReference(entry, link.getAttribute("href"));
  const style = rewritten.window.document.createElement("style");
  style.textContent = rewriteCss(resources.get(resolved).body.toString("utf8"), resolved);
  link.replaceWith(style);
}
const bridgeSource = `
(function () {
  const api = {
    version: 2,
    plugin: Object.freeze({ name: "nmbb.codex-candy-eval" }),
    page: Object.freeze({ id: "codex-candy-eval", title: "Codex 降智与指纹测试" }),
    theme: Object.freeze({ name: "dark", tokens: {} }),
    resourceUrl(path) { throw new Error("页面不应直接加载资源：" + path); },
    callbackTicket() { return Promise.reject(new Error("未使用")); },
    models: Object.freeze({ responses() { return Promise.reject(new Error("未使用")); } }),
    request(input) {
      const call = { method: String(input.method || "").toUpperCase(), path: String(input.path || ""), query: input.query == null ? "" : String(input.query), body: input.body ? JSON.parse(new TextDecoder().decode(input.body)) : null };
      window.__calls.push(call);
      return Promise.resolve(window.__backend(call)).then((answer) => ({
        status: answer.status,
        contentType: "application/json",
        body: new TextEncoder().encode(JSON.stringify(answer.body)).buffer,
      }));
    },
  };
  Object.defineProperty(window, "codexProxyPlugin", { value: Object.freeze(api), configurable: false, writable: false });
  document.documentElement.dataset.theme = "dark";
})();
`;
const bridge = rewritten.window.document.createElement("script");
bridge.textContent = bridgeSource;
rewritten.window.document.head.prepend(bridge);
const srcdoc = `<!doctype html>\n${rewritten.window.document.documentElement.outerHTML}`;

const errors = [];
const dom = new JSDOM(srcdoc, {
  runScripts: "dangerously",
  pretendToBeVisual: true,
  url: "https://plugin.invalid/",
  beforeParse(window) {
    window.__calls = calls;
    window.__backend = mockBackend;
    // jsdom lacks <dialog>; exercise the non-modal compatibility path, not sandbox permissions.
    const dialogProto = window.HTMLDialogElement.prototype;
    dialogProto.show = function () { this.setAttribute("open", ""); };
    dialogProto.showModal = function () { throw new Error("Native modal intentionally unavailable in this fixture"); };
    dialogProto.close = function () { this.removeAttribute("open"); this.dispatchEvent(new window.Event("close")); };
    window.__viewportObservations = 0;
    window.__viewportDisconnects = 0;
    window.IntersectionObserver = class {
      constructor(callback) { this.callback = callback; }
      observe() {
        window.__viewportObservations++;
        this.callback([{ intersectionRect: { top: 80, left: 20, height: 480, width: 640 } }]);
      }
      unobserve() {}
      disconnect() { window.__viewportDisconnects++; }
    };
    // jsdom 未实现 CSS.escape；浏览器里始终存在，这里给一个等价的属性值转义实现。
    window.CSS = { escape: (value) => String(value).replace(/[^a-zA-Z0-9_-]/g, (ch) => "\\" + ch) };
    // jsdom 未实现 matchMedia；浏览器里始终存在，这里补一个最小替身。
    window.matchMedia = (query) => ({
      matches: false, media: query, onchange: null,
      addEventListener() {}, removeEventListener() {},
      addListener() {}, removeListener() {}, dispatchEvent() { return false; },
    });
    window.addEventListener("error", (event) => errors.push(String(event.error?.stack || event.message)));
    window.addEventListener("unhandledrejection", (event) => errors.push(`未处理的 Promise 拒绝：${event.reason}`));
    // 沙箱页面没有网络与存储，任何直接使用都是缺陷。
    for (const name of GLOBALS_BLOCKED) {
      Object.defineProperty(window, name, {
        configurable: true,
        get() { throw new Error(`沙箱页面不应直接使用 ${name}`); },
      });
    }
  },
});

const wait = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
await wait(3000);

const doc = dom.window.document;
const $ = (id) => doc.getElementById(id);

for (const error of errors) fail(`页面抛出异常：${error}`);
check($("app") && !$("app").hidden, "主界面未显示");
check($("login") && $("login").hidden, "不应要求管理密钥登录");
check(!$("hourly-candy").hidden && $("hourly-candy").textContent.includes("每小时自动测试") && $("hourly-candy").textContent.includes("下次"), "自动糖果状态必须显示启用配置与下次调度时间");
check(doc.documentElement.dataset.cprPluginHost === "", "未标记 CPR 宿主，主题样式不会生效");
check(['0', '0px'].includes(dom.window.getComputedStyle(doc.querySelector('main')).minHeight), "CPR 自动高度页面不得保留视口最小高度");

const options = [...$("model").options].map((option) => option.value);
check(options.includes("gpt-6.1-sol"), `模型下拉缺少目标模型：${JSON.stringify(options)}`);

const rows = $("rows")?.children?.length ?? 0;
check(rows === 1, `凭证行数量应为 1，实际 ${rows}`);
// 控制台用邮箱作为凭证标题，与 CPA 面板一致。
check(($("rows")?.textContent || "").includes("a@example.invalid"), "凭证行未渲染账号标识");
check((dom.window.document.body.textContent || "").includes("CPR 账号"), "账号筛选未使用 CPR 来源标签");
// 糖果判定与最近记录都在行内渲染。
check(!!$("rows")?.querySelector(".verdict.ok"), "糖果判定未渲染为答对");
// 行内汇总：判定、耗时与正确率。
const rowText = $("rows")?.textContent || "";
check(rowText.includes("答对"), "糖果判定未渲染");
check(rowText.includes("67%"), "糖果正确率未渲染");

// 页面只保留糖果、ModelTrace、鹈鹕三套件；指纹页签已下架，不能重新出现在沙箱里。
check(!$("tab-fingerprint"), "指纹页签应已从页面移除");
for (const suite of ["modeltrace", "pelican", "candy"]) {
  $("tab-" + suite)?.dispatchEvent(new dom.window.MouseEvent("click", { bubbles: true }));
  check(!$(suite + "-panel")?.hidden, `${suite} 面板未显示`);
  check($("tab-" + suite)?.getAttribute("aria-selected") === "true", `${suite} 页签未选中`);
}

// 非模态兼容路径、可见区域定位与失败原因展示；真实布局另用 Chrome OOPIF 回归。
$("tab-modeltrace")?.dispatchEvent(new dom.window.MouseEvent("click", { bubbles: true }));
await wait(50);
$("mt-rows")?.querySelector("[data-row]")?.dispatchEvent(new dom.window.MouseEvent("click", { bubbles: true }));
await wait(50);
const detailButton = $("mt-rows")?.querySelector("[data-mt-detail]");
check(!!detailButton, "ModelTrace 历史记录缺少查看详情按钮");
detailButton?.dispatchEvent(new dom.window.MouseEvent("click", { bubbles: true }));
await wait(400);
check($("mt-detail")?.open === true, "查看详情未能打开详情对话框");
check(!!doc.querySelector(".dialog-backdrop"), "非模态对话框缺少遮罩");
check(($("mt-detail-body")?.textContent || "").includes("no upstream provider"), "详情未显示失败原因");
check($('mt-detail').style.getPropertyValue('--dialog-top') === '320px', "弹窗未按可见区域居中");
check($('mt-detail').style.getPropertyValue('--dialog-max-height') === '448px', "弹窗未限制在可见区域内");
$("mt-detail-close")?.dispatchEvent(new dom.window.MouseEvent("click", { bubbles: true }));
await wait(20);
check($("mt-detail")?.open === false, "关闭详情后对话框仍处于打开状态");
check(!doc.querySelector(".dialog-backdrop"), "关闭后遮罩残留");
check(dom.window.__viewportDisconnects === 1, "关闭详情未释放可见区域观察器");
const observationsAfterClose = dom.window.__viewportObservations;
await wait(180);
check(dom.window.__viewportObservations === observationsAfterClose, "关闭后仍在轮询弹窗几何");
check(!$('mt-detail').style.getPropertyValue('--dialog-top'), "关闭后遗留弹窗定位");
const callsSeen = new Set(calls.map((call) => `${call.method} ${call.path}`));
for (const expected of [
  "GET api/accounts",
  "GET api/models",
  "GET api/state",
]) {
  check(callsSeen.has(expected), `页面未调用预期路由：${expected}`);
  check(![...callsSeen].some((route) => route.includes("v0/management") || route.includes("v8/management") || route.includes("credentials/sync")), "新页面不应再依赖 CPA 管理接口别名");
}

const registered = new Set(JSON.parse(readFileSync(path.join(root, "tests", "routes.json"), "utf8")).map(([method, route]) => `${method} ${route}`));
for (const key of callsSeen) check(registered.has(key), `后端未注册页面调用的路由：${key}`);

// 通知必须使用真实数组契约，不能把空数组当作跳过、告警或可展示的 ID 文本。
const noticeFor = (response) => dom.window.eval(`runResponseNotice(${JSON.stringify(response)})`);
const empty = noticeFor({ started: 1, skipped: [], unchecked: [], busy: [] });
check(empty.tone === "info", "全量执行完成不能被空数组染成黄色告警");
check(empty.message.includes("已结束") && !empty.message.includes("开始测试"), "同步完成响应不能被标为开始测试");
check(!/跳过|无法读取|仍会尝试/.test(empty.message), "空数组不能产生跳过或未知模型警告");
for (const field of ["skipped", "unchecked", "busy"]) {
  const notice = noticeFor({ started: 1, skipped: [], unchecked: [], busy: [], [field]: ["private-one", "private-two"] });
  check(notice.tone === "warning" && notice.message.includes("2 个凭证"), `${field} 应显示数组长度及真实警告`);
  check(!notice.message.includes("private-"), `${field} 不得把账号 ID 拼到通知中`);
  if (field === "unchecked") check(notice.message.includes("已跳过") && !notice.message.includes("仍会尝试"), "未确认模型的实际跳过行为不能写成仍会尝试");
}
const zero = noticeFor({ started: 0, skipped: [], unchecked: [], busy: [] });
check(zero.tone === "warning" && zero.message.includes("未执行"), "无执行项不能宣称测试已结束或已启动");
const numeric = noticeFor({ started: 2, skipped: 0, unchecked: 0, busy: 1 });
check(numeric.tone === "warning" && numeric.message.includes("1 个凭证已有任务"), "数字计数兼容路径失效");
const invalid = noticeFor({ started: 1, skipped: -1, unchecked: "private-id", busy: {} });
check(invalid.tone === "info" && !invalid.message.includes("private-id"), "异常计数字段不应回显标识或产生虚假告警");

const until = async (predicate, label) => {
  const deadline = Date.now() + 4000;
  while (!predicate() && Date.now() < deadline) await wait(10);
  check(predicate(), label);
};
const flash = () => $("flash").querySelector(".notice-message").textContent;
const runButton = $("run-batch");
check(runButton && !runButton.disabled, "测试按钮应可用");
for (const queued of [false, true]) {
 for (const route of ["/run", "/modeltrace/run", "/pelican/run", "/fingerprint/run"]) {
  mockRunResponse = { queued, started: 1, skipped: [], unchecked: [], busy: [] };
  let finish;
  mockRunGate = new Promise((resolve) => { finish = resolve; });
  const before = calls.filter((call) => call.path === "api" + route).length;
  let action;
  if (route === "/run") runButton.dispatchEvent(new dom.window.MouseEvent("click", { bubbles: true }));
  else action = dom.window.eval(`update(${JSON.stringify(route)}, { method: "POST", body: { auth_ids: ["acct-1"], model: "gpt-6.1-sol" } }, "开始测试失败")`);
  check(!$("flash").hidden && flash().includes("正在准备"), `${route} 点击后应立即显示准备提示`);
  await until(() => calls.filter((call) => call.path === "api" + route).length > before, `${route} 未提交运行请求`);
  const submitted = calls.filter((call) => call.path === "api" + route).at(-1);
  check(new URLSearchParams(submitted.query).get("background") === "1" && !new URLSearchParams(submitted.query).has("__cpr_background"), `${route} 必须使用插件后台任务，不能依赖宿主补丁`);
  check($("flash").dataset.tone === "info" && flash().includes("等待启动确认"), `${route} 响应前应显示普通准备提示`);
  check(!flash().includes("已结束"), `${route} 响应前不能提前宣称测试结束`);
  check(runButton.disabled, `${route} 运行期间测试按钮应禁用`);
  await dom.window.eval('runCandy({ all: true })');
  check(calls.filter((call) => call.path === "api" + route).length === before + 1, `${route} pending 期间不得重复提交`);
  finish();
  if (action) await action;
  await until(() => !runButton.disabled, `${route} 完成后未恢复按钮`);
  check(flash().includes(queued ? "已开始后台测试" : "已结束") && $("flash").dataset.tone === "info", `${route} 必须区分后台接收和同步结束`);
  if (queued) check(!flash().includes("已结束"), `${route} 后台接收不能误报完成`);
  check(!flash().includes("跳过") && !flash().includes("仍会尝试"), `${route} 完成后仍包含空数组的伪警告`);
}
}
for (const [field,value,type,path] of [["running",{done:0,total:1},"candy","api/cancel"],["pelican_running",true,"pl","api/pelican/cancel"],["fingerprint_running",{done:0,total:1},"fp","api/fingerprint/cancel"],["modeltrace_running",{done:0,total:1},"mt","api/modeltrace/cancel"]]) {
  mockRunning = {[field]:value};
  await dom.window.eval('load()');
  const button = dom.window.document.querySelector(`[data-${type}-cancel="acct-1"]`);
  check(button && !button.disabled, `${type} 后台运行时必须提供停止按钮`);
  const before = calls.filter((call) => call.path === path).length;
  button?.dispatchEvent(new dom.window.MouseEvent("click", {bubbles:true}));
  await until(() => calls.filter((call) => call.path === path).length > before, `${type} 停止按钮未调用取消接口`);
  check(calls.filter((call) => call.path === path).at(-1)?.body?.id === "acct-1", `${type} 停止请求必须使用后端 RecordQuery.id，不能只验证路由名称`);
  await until(() => !dom.window.document.querySelector(`[data-${type}-cancel="acct-1"]`), `${type} 停止后按钮未恢复`);
}
mockRunGate = null;
mockRunStatus = 503;
mockRunResponse = { error: { message: "模拟执行请求失败" } };
await dom.window.eval('runCandy({ all: true })');
check($("flash").dataset.tone === "error" && flash().includes("模拟执行请求失败"), "请求失败必须保留真实错误提示");
check(!flash().includes("已结束"), "失败请求不能生成完成提示");
mockRunStatus = 200;
mockHourly = { ...mockHourly, last_dispatch: { status: "dispatch_error" } };
await dom.window.eval('load()');
check($("hourly-candy").textContent.includes("上次调度失败"), "自动调度失败不能显示为正常完成");
mockHourly = { enabled: false };
await dom.window.eval('load()');
check($("hourly-candy").textContent.includes("未启用"), "关闭自动测试后必须显示停用状态");
for (const call of calls) {
  const route = `${call.method} ${call.path}`;
  check(registered.has(route), `未注册的运行/轮询路由：${route}`);
  if (call.method !== "POST" || !call.path.endsWith("/run")) {
    check(!new URLSearchParams(call.query).has("background") && !new URLSearchParams(call.query).has("__cpr_background"), "短管理请求不得默认改成后台执行");
  }
  callsSeen.add(route);
}
for (const error of errors) fail(`页面抛出异常：${error}`);
// 关闭页面轮询与三个 jsdom 窗口，避免校验通过后进程仍常驻。
dom.window.close();
rewritten.window.close();
document.defaultView.close();
if (failures.length) {
  console.error("页面装配/渲染校验失败：");
  for (const failure of failures) console.error("  - " + failure);
  process.exit(1);
}
console.log(`页面校验通过：资源 ${resources.size} 项，桥调用 ${calls.length} 次，凭证行 ${rows} 行，模型 ${options.length} 个`);
console.log("调用的路由：" + [...callsSeen].sort().join(", "));
