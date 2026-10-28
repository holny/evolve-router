# 模型优先级 7 档（面板 + Web 双入口，同源持久化）

## 档位定义（-3..+3，只在 ≠0 时落盘）

| level | EN | ZH | 色点 | 路由语义 |
|---|---|---|---|---|
| -3 | Off | 不启用 | gray | 自动路由永不选中（显式指定仍可用） |
| -2 | Lowest | 低优先级 | violet | 总分 ×0.40 |
| -1 | Lower | 较低优先级 | blue | 总分 ×0.70 |
| 0 | Default | 不干预 | 无 | 公式决定（默认，不存盘） |
| +1 | Higher | 较高优先级 | teal | 总分 ×1.30 |
| +2 | Highest | 高优先级 | amber | 总分 ×1.60 |
| +3 | Pinned | provider内必使用 | green | 必选：强制入候选集（绕过活跃池/心选范围 + 质量下限 + 价位闸），总分 ×1.90；provider 健康/配额/窗口等硬约束仍生效；同 provider 唯一，重复设置时旧 pin 自动降为 +2 |

- **乘法档位（2026-10-28 用户裁决，取代旧"加性带距 100/档碾压"）**：档位按百分比
  缩放总分（±30%/±60%，+3 ×1.9）——跨档不再无条件碾压，难题上公式分显著更优的
  未抬档模型仍可胜出；正档位只有 +3 豁免质量下限（+1/+2 不再豁免）。
- 心选路径（Favorites）的心选模型默认享受 ×1.30 软加成——只乘分数，**不进档位
  语义**（粘性接管/低置信留旧/缓存经济仍读显式档位：设心选不打断进行中会话）；
  显式 ▲▼ 调档完全取代软加成。
- 不可抗拒力（健康冷却 / 配额耗尽 / plan 预算 / provider 降级 / 窗口不足 / 无凭据）仍正常过滤——priority 不豁免这些。

## 数据源与同步

- 单一事实源：`overrides.json` 的 `priorities: HashMap<String, i8>`（0 值不存）。
- 热状态：`Engine.priority_overlay`（RwLock），启动时从 overrides.json 重放（state.rs 重放块）。
- 双入口：面板与 dashboard 都经 `POST /api/priority {model, level}` 写入（引擎热生效 + 落盘），都从 `/api/model-tags`（每行新增 `priority` 字段）读取——轮询天然同步。

## 引擎挂钩点（engine.rs）

1. 候选循环：`prio <= -3` → filtered("user priority: Off")。
2. 活跃池/范围过滤：`prio == 3` 豁免。
3. S2 价位闸：`prio == 3` 视同 tier_ok。
4. 质量下限：`band >= 1` → 豁免（2026-10-28 起仅 +3 Pinned 可达，+1/+2 改乘法后不再进入此分支）。
5. sticky 快路径：`sticky.chosen` 为 Off 时不再粘（重评估走新档位）；正档位接管粘性的比较读显式档位（心选软加成不参与）。
6. 排序前（软惩罚之后）：`score *= band_factor(band)`（band_factor: ±1→1.3/0.7，±2→1.6/0.4，+3→1.9）。

## API（meta.rs）

- `POST /api/priority {model, level}`：校验；level==3 时同 provider（m.provider）旧 pin 降为 2 并在响应 `demoted` 里返回；level==0 清除覆盖；热生效 + save_overrides。
- `/api/model-tags` 行加 `priority`。

## UI

- dashboard（renderTags 行）：模型名后加 7 档色点 + ▲▼ 微型步进钮；列表下方加 7 色图例（与现有 legend 风格一致）；i18n 6 语言 key。
- 面板（PanelView.modelRow）：色点 + ▲▼；`modelTagLegend` 旁加优先级图例；I18n.swift 加 key；AppState.ModelRow 加 priority 字段 + setPriority()。

## 测试

- engine：Off 过滤 / Pinned 绕活跃池与价位闸 / 跨档排序 / 同档公式序。
- meta：同 provider 旧 pin 降档；overrides 持久化 round-trip。
