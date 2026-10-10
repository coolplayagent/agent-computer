# 53. 内部 Browser Driver

`packages/browser-driver` 实现 D09 中的 TypeScript/Playwright 驱动组件：每个获准启动的 Browser App 实例使用独立 Chromium profile，支持多页、有界观察和显式动作。这是内部 worker 库。公开 Browser API/CLI、App 启动接入、GUI 控制租约和 ComputerView 尚未接入，服务仍声明 Browser 不受支持。

## Worker 契约

`BrowserDriver.launch` 必须接收可信 `Authority`、`ActionJournal`、本地私有 `profile_root`、视口、运行预算、调用者以及组织/Computer/App 代次/profile 主体范围。这些是 worker 输入，不是客户端提供的回调或文件路径。Authority 每次检查当前身份、连接、授权、App/profile ACL、代次和共享控制 epoch；启动还需持久激活和资源准入。Journal 必须在派发前原子保留范围内的动作 ID，重复且未解决的 intent 绝不能再次返回派发许可。测试适配器不提供这些生产保证。

驱动新建私有 profile 目录，使用配套 Chromium 并开启其沙箱；浏览器环境不继承宿主凭据。不接管用户日常 profile，不接收任意 Chromium 参数，不公开 CDP/脚本执行，不把共享 Candidate 挂载为 profile。外层 App 沙箱仍需网络策略、CPU/内存/PID/磁盘限额、凭据隔离和独立运行期限守卫。HTTP(S) 导航校验不构成出口或 SSRF 策略；页面仍可加载资源、执行脚本及创建标签页。DOM 元数据是不可信页面内容，不能证明授权；Node 再次校验其形状与限额，恶意渲染器行为仍依赖外层进程与资源边界。

## 观察与动作

`pages(actor)` 和 `observe(actor, page_id)` 在浏览器读取前后检查在线读权限。观察包含范围、随机页面/观察 ID、文档代次、控制 epoch、采集时间、剩余有效期、CSS 视口、PNG 尺寸、设备倍率、图像倍率及 SHA-256。图片坐标为 CSS 视口，倍率为 1。截图上限 2 MiB；DOM 最多遍历 2,000 个元素，返回 100 个可操作元素引用和 32 KiB 渲染端元数据。引用持有实际元素句柄，不接收客户端 selector。遍历不采集密码值、cookie 或 storage；可见像素和标签仍可能含敏感信息，因此需要读权限。

支持 `create_page`、`close_page`、`navigate`、`click`、`click_at`、`type`、`select`、`key`、`scroll`。除新建页面外，每个动作都需该页的有效观察。DOM 动作复核引用元素、可见性、位置与有界描述；坐标点击还要求新截图字节完全一致。按键要求焦点元素相同，只接受有限按键集合。任何动作均不接收任意脚本。复核不能冻结检查与实际输入之间的页面变化；新鲜观察和 `delivered` 回执不证明预期业务效果。导航/框架变化、控制 epoch 变化及已派发动作使观察失效；视口/滚动位置、目标几何变化和过期引用在新 intent 准入前被拒绝。观察最多有效 30 秒，还受实际授权与操作期限限制，当前实现通常更短。

同一实例只允许一个操作在途。上限为八页、16 份观察、1,024 条动作记录、单操作 10 秒、输入文本 16 KiB、获准运行五分钟。超额页面会关闭；这限制保留页数，不能阻止页面此前已发出的请求。到期开始关闭 context，结果为 `closed` 或 `close_unknown`；即使 `closed` 也不是节点/cgroup fence，不授权转移 profile 所有权。profile 保留给后续可信采集、加密和 checkpoint 流程，本增量不恢复或发布 profile。

## 动作结果不确定性

`act(actor, action_id, action)` 的摘要绑定范围、调用者、控制 epoch 和规范化动作。驱动校验观察、持久 intent、复核当前授权和目标，再派发一次。浏览器明确应答得到 `delivered`，不代表业务效果成功。intent 准入后的失败记为 `unknown`，阻止新的控制动作。结果持久化应答丢失返回 `recorded=false` 和 `unknown`，不覆盖可能已经提交的回执。在仍有授权时，相同本地重试返回原结果；相同 ID 的输入变化被拒绝。Unknown 不自动重放。

操作超时会关闭 context，也可能只返回错误而没有回执。只要动作已准入，调用者就必须按不确定处理并核对持久 journal。本库尚无跨进程 journal 恢复 API；后续适配器必须核对准确动作身份和原物理运行环境。重启驱动或换动作 ID 不能证明旧动作未发生。不确定完成后，若 context 仍打开且读权限有效，仍可观察。

## 构建与验证

固定 Node 22.22.1、TypeScript 5.9.3、pnpm 9.15.9、Playwright Core 1.64.0，使用原生 Bazel JS/TS 规则：

```sh
bazel test //packages/browser-driver:unit_test
npm exec --yes --package=pnpm@9.15.9 -- pnpm install --frozen-lockfile --ignore-scripts
npm run build
PLAYWRIGHT_BROWSERS_PATH=/private/test-browsers node node_modules/playwright-core/cli.js install chromium
PLAYWRIGHT_BROWSERS_PATH=/private/test-browsers bazel test //packages/browser-driver:browser_test --test_env=PLAYWRIGHT_BROWSERS_PATH
```

安装和测试需使用同一浏览器路径。浏览器及系统库属于显式环境准备，构建不会通过包安装脚本下载浏览器。手动标记的 browser target 需要配套 Chromium 和可用沙箱；缺失时失败，不跳过、不自动关闭沙箱。普通 `bazel test //...` 包含 unit target。

测试使用真实页面和 HTTP 效果，覆盖 PNG/DOM、结构化及坐标输入、多页、撤权、导航/框架/控制切换失效、边界、回执丢失和迟到的 journal 准入。内存授权/journal fixture 仅验证驱动行为。完整 App 生命周期、持久 GUI 授权/journal、加密 profile checkpoint、跨节点 fencing、ComputerView 和 T04/T05/T10/T24 验收仍未完成。

[绑定源码的组件证据](../evidence/browser-driver-2026-10-11.json)与[真实浏览器测试输出](../evidence/browser-driver-2026-10-11.log)记录一次性 VM 中同时启用 gVisor 和 Chromium 沙箱的 11 项通过结果，不构成 App 生命周期或产品验收。

[App 持久化准入](54-app-admission.md)现已提供独立预留接口，尚未实现本驱动所需的 Authority/journal 回调接入。

[Browser worker 桥接](55-browser-worker-bridge.md)现提供私有进程通信入口，持久控制器和节点接入仍待完成。
