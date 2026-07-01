# outline-mcp — Project Rules

## 最上位 (作業開始前に必ず読む)

**outline-mcp Book (`mcp__outline__select_book(book="outline-mcp")` → `toc`) を先に確認する**。 過去に踏んで対策が確定している pitfall はここに体系化されている。 これを読まずに release / publish / workspace 変更 / build 経路変更 に着手すると、 既知の trap を再度踏む (「N 回目です」 系事故の直接原因)。

対象 phase (最低限):
- publish 準備 (bump / cargo publish / gh release / MCP Registry)
- workspace layout 変更 (crate 分割 / 統合 / member 追加)
- Dockerfile / build context 変更
- `include_str!` / `include_bytes!` を新規追加 / path 変更
- MCP tool / resource の追加 / 変更

## Book 参照 SOP

```
mcp__outline__select_book(book="outline-mcp")
mcp__outline__toc(book="outline-mcp")
```

TOC を眺めて該当項目があれば node 本文を Read してから着手する。

新規 pitfall (再発性のある事故) は Book に append する (`node_create` の `content` type、 症状 / 真因 / 正しい配置 / Check の 4 節構成が既存 pattern)。

## その他

上位 rule (cc-x/.claude/CLAUDE.md、 ~/.claude/CLAUDE.md、 users/rules/*) はそのまま継承する。 本 file は outline-mcp 固有の trap catalog 参照 mandate のみ担う。
