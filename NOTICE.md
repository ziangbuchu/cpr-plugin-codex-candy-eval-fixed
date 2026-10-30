# 来源与第三方许可说明（NOTICE）

本仓库是 **CPR（codex-proxy-rs）平台上的魔改移植版**，不是上游原作者发布的版本。

## 一、上游项目

| 项目 | 地址 | 许可 | 本仓库如何使用 |
| --- | --- | --- | --- |
| `cpa-plugin-codex-candy-eval`（Go/CGO 动态库插件） | https://github.com/haowang02/cpa-plugin-codex-candy-eval | MIT，Copyright (c) 2026 Hao Wang | 功能语义、判定规则、页面交互与题面口径的原型；本仓库为 CPR 平台用 Rust 重写，并在此基础上修改 |

上游 MIT 许可与版权声明完整保留在根目录 [LICENSE](LICENSE)。

## 二、随仓库分发的第三方数据与算法

| 来源 | 许可 | 位置 |
| --- | --- | --- |
| [ModelTrace](https://github.com/xqy2006/ModelTrace)，固定提交 `df3a0f9d3e054c0dc02d6d586686db8daf8fa7c8` | MIT，Copyright (c) 2026 xqy2006 | `data/modeltrace/unified_bank.json`、参照其 `static/fingerprint-core.js` 与 `static/challenge-browser.js` 的评分与题目生成算法（Go → Rust 移植）；许可原文见 `data/modeltrace/LICENSE`，说明见 `data/modeltrace/README.md` |

上述数据文件在编译期由 `include_bytes!`/`include_str!` 嵌入插件二进制，因此是构建必需品，不可删除。

## 三、前端第三方库（`web/vendor/`）

以下文件按其自带头部声明分发，原始许可归各自作者：

| 文件 | 版本 | 许可 |
| --- | --- | --- |
| `markdown-it.min.js` | 15.0.2 | MIT |
| `markdown-it-cjk-friendly.min.js` | 3.0.0（含 `get-east-asian-width` 1.7.0） | MIT |
| `temml.min.js` / `temml.css` | 0.13.5 | MIT |
| `texmath.js` | — | MIT（Copyright (c) Stefan Goessner） |
| `temml-*.woff2` / `temml-*.svg` | 随 Temml 发行包 | Temml 发行包以 MIT 提供；`Temml.woff2` 为 `KaTeX_Script-Regular` 的克隆，仅改动码点 |

## 四、商标与免责

- 本项目与 OpenAI、Codex、CPA（CLIProxyAPI）、CPR（codex-proxy-rs）各方均无隶属或背书关系。
- 判定结果（是否“降智”、模型指纹、模型归因）均为统计性参考，**不构成对模型身份或服务质量的证明**；采集条件、同类模型行为与上游改动都可能影响结论。
