# Codex 个人账号原始 Credits 统计

本次调整参考 Codex Quota Compass 3.10.1 的个人模式，将原始 Credits 与
Token 价格估算分开。没有移植脚本默认启用的 Astra 缓存校正。

## 数据来源与计算

- 个人账号：读取官方日合计的 `totals.credits`，模型明细为接口绝对值或按同日权重还原。
- Team：继续使用模型级 Token、本地模型价格表及额度速度倍率。
- 本地日志：继续使用 JSONL 的 Token 和服务档位，保留缓存写入记账。
- 个人原始 Credits 不再额外乘 Fast 倍率，不需要价格表，也不反推模型 Token。
- 美元等值 = 原始 Credits / 自定义换算系数。默认 25 Credits / USD，可在卡片中调整；
  这不是官方账单汇率，也不是订阅实付金额。
- 100% 周期容量 = 已同步日合计 / 官方已用比例，不是期末实际消耗预测。
- 额度消耗速度和耗尽时间继续使用独立的额度采样，保持账号及周期隔离。

## 数据质量与降级

校验日合计的 `balance_unit=credit`、`group_by=day` 和模型明细的
`units=percent|credit|credits`。重复/无效日期、无效值及绝对明细合计不一致
不会被默认为零或强行分配。

原始日合计、Token 和模型归属独立展示；缺失 Credits 暂停 Credits/USD 容量外推，
缺失 Token 暂停 Token 容量外推。模型明细失败不影响有效日总额。
analytics 失败也不会丢弃同次查询已成功取得的额度快照。
未知单位或数据结构时显示明确提示，不静默切回 Token 价格估算或本地数据。

日桶按 UTC 解释并与精确额度周期求交集，边界日计入整日，可能包含周期外用量。
接口未声明日桶时区，统计也可能延迟；这些限制不会因采用原始 Credits 而消失。

## Astra Fast 与历史本地记录

官方 Credits 模式的 Astra Fast 倍率为 Standard 的 2.5 倍；前端 Token 估算和
本地日志导入均已补齐。不要将此倍率混同于 API Key 的实际账单定价。

已经导入的旧记录不会通过一次代码更新自动重算。本次不自动删除或重建历史数据库。
需要修正历史 Astra Fast 记录时，可以在用量页面使用已有的 Codex 安全重建操作；
它从仍存在且通过审计的原始日志重新解析，无法验证的历史会保留。
个人官方 Credits 模式不依赖这些本地记录，刷新即可读取原始日统计。

## 验证

常规回归不联网。手动接口契约检查使用当前 Codex 登录，只输出状态和条数：

```sh
cargo test --manifest-path src-tauri/Cargo.toml --locked --lib \
  services::codex_official_usage::tests::live_personal_credit_contract \
  -- --ignored --exact --nocapture
```

参考：[官方 Credits 费率及 Astra Fast 说明](https://learn.chatgpt.com/docs/pricing#token-rates)。
