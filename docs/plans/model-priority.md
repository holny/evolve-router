# 模型优先级 7 档（面板 + Web 双入口，同源持久化）

## 档位定义（-3..+3，只在 ≠0 时落盘）

| level | EN | ZH | 色点 | 路由语义 |
|---|---|---|---|---|
| -3 | Off | 不启用 | gray | 自动路由永不选中（显式指定仍可用） |
| -2 | Lowest | 低优先级 | violet | 只有 pool 里没有更好档位时才用（band −200） |
| -1 | Lower | 较低优先级 | blue | band −100 |
| 0 | Default | 不干预 | 无 | 公式决定（默认，不存盘） |
| +1 | Higher | 较高优先级 | teal | band +100 |
| +2 | Highest | 高优先级 | amber | band +200 |
| +3 | Pinned | provider内必使用 | green | 强制入候选集（绕过活跃池 + 价位闸）；provider 健康/配额/窗口等硬约束仍生效；同 provider 唯一，重复设置时旧 pin 自动降为 +2 |

- 加性带距 100/档 ≫ 公式分差与 Thompson 扰动（±3%）：跨档档位定胜负，同档内公式照常竞争。
- 不可抗拒力（健康冷却 / 配额耗尽 / plan 预算 / provider 降级 / 窗口不足 / 无凭据）仍正常过滤——priority 不豁免这些。

## 数据源与同步

- 单一事实源：`overrides.json` 的 `priorities: HashMap<String, i8>`（0 值不存）。
- 热状态：`Engine.priority_overlay`（RwLock），启动时从 overrides.json 重放（state.rs 重放块）。
- 双入口：面板与 dashboard 都经 `POST /api/priority {model, level}` 写入（引擎热生效 + 落盘），都从 `/api/model-tags`（每行新增 `priority` 字段）读取——轮询天然同步。

## 引擎挂钩点（engine.rs）

1. 候选循环：`prio <= -3` → filtered("user priority: Off")。
2. 活跃池过滤（:511）：`prio == 3` 豁免。
3. S2 价位闸（:663 keep 闭包）：`prio == 3` 视同 tier_ok。
4. sticky 快路径：`sticky.chosen` 为 Off 时不再粘（重评估走新档位）。
5. 排序前（:833 之前，软惩罚之后）：`score += 100 * band`。

## API（meta.rs）

- `POST /api/priority {model, level}`：校验；level==3 时同 provider（m.provider）旧 pin 降为 2 并在响应 `demoted` 里返回；level==0 清除覆盖；热生效 + save_overrides。
- `/api/model-tags` 行加 `priority`。

## UI

- dashboard（renderTags 行）：模型名后加 7 档色点 + ▲▼ 微型步进钮；列表下方加 7 色图例（与现有 legend 风格一致）；i18n 6 语言 key。
- 面板（PanelView.modelRow）：色点 + ▲▼；`modelTagLegend` 旁加优先级图例；I18n.swift 加 key；AppState.ModelRow 加 priority 字段 + setPriority()。

## 测试

- engine：Off 过滤 / Pinned 绕活跃池与价位闸 / 跨档排序 / 同档公式序。
- meta：同 provider 旧 pin 降档；overrides 持久化 round-trip。
