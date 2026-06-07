Composable 负责页面级状态、API 编排、确认弹窗、toast、任务轮询和错误归一化。不要在组件中重复这些流程。

新增异步操作应维护 loading/error 状态，并在成功后刷新相关页面数据。涉及导入、恢复、删除、覆盖等风险操作时必须先确认。

Composable 可以包含轻量产品状态映射函数，但不应包含后端文件操作规则；真实业务规则、安全边界和持久化语义应由后端保证。

调用 Tauri API 时应通过 `src/api.ts` 封装，不要在 composable 中直接散落 `invoke`。

修改 composable 后，至少运行 `npm run build` 验证 TypeScript 类型、组件传参和事件链路。
