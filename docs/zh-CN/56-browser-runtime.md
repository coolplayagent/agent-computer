# 56. Browser Pod 与节点运行时

[私有 worker 桥接](55-browser-worker-bridge.md)现已增加 Rust 控制器协议、Kubernetes Browser 计划/attach 适配器和独立节点进程保护。这些属于内部组件；公开 App API 仍只持久化排队准入，生产派发、GUI 持久授权/动作日志与 profile checkpoint/恢复仍待接入，Browser readiness 不变。

## 独立 Pod

`BrowserPlan` 绑定已准入的组织、Computer/App 代次、主体、profile owner、viewport、剩余运行期限、批准镜像 digest 及固定 Node UID/boot。Pod 无 Candidate/PVC 挂载或 ServiceAccount token，使用 gVisor、非 root、空 capabilities、只读根、固定 CPU/内存限额和部署的 deny-all 策略。计划可供审查，不构成创建授权。

批准镜像提供 `/usr/local/bin/node`、编译后的 `/opt/agent-computer/browser-driver/worker.js` 和匹配的 Playwright/启用 sandbox 的 Chromium。同一镜像以非 root init container 执行 `--prepare-profile-volume`，在 512 MiB 内存卷创建全新的 0700 私有目录，再以 subPath 挂载到主容器 `/browser-state`。已有目录、错误 owner 或符号链接根会导致准备失败。`/tmp` 使用独立 512 MiB 内存卷，`/dev/shm` 为 64 MiB。profile 当前为易失数据。

严格回读核对 manifest、成功且未重启的 init、运行中的主容器、Pod UID 和 Node UID/boot。仅允许已知的 Kubernetes init 默认字段，未知安全字段仍拒绝。内存量使用 Kubernetes 规范表示，避免 `4096Mi` 被 API 转成 `4Gi` 后产生错误的身份冲突。

## 私有控制器与活节点保护

适配器只建立一次 Kubernetes v5 attach，完成 nonce 握手后再次核对原运行身份，不重连、不重发写入。Rust `Session` 限制字节、帧、请求和耗时，在调用必需的 `Policy` 前，逐项比对 callback ID、scope、actor、permission、epoch 和完整动作输入。读结果不得跨控制 epoch；截图核对有限 base64、PNG 头/尺寸及 SHA-256，DOM 始终作为有限的不可信观察处理。动作完成不确定会禁止新控制并保留原重试身份。

`Policy` 必须认证原调用者，核对当前数据库授权和活节点保护，在 dispatch 前持久化唯一 intent，在完成应答前持久化结果。协议字符串或测试回调不能提供这些权限。生产调用方还须重新核对发布权限；本组件没有提供 PostgreSQL policy 适配器。

`arm_browser` 核对真实 CRI/containerd/runsc 身份、profile subPath 的私有 tmpfs inode、原进程起始时间、cgroup 与 pidfd；在允许 launch 回调前布防两个独立 watchdog 并取得 reaper admission。共享保护机制保留既有 Candidate API；Browser 使用独立证据类型，不能生成 Candidate IO seal。

`stop_browser` 终止并确认原进程域停止，保持原私有 profile inode 句柄，并在私有节点 spool 原子记录独立的 `browser-stop` 凭据。它只说明历史进程停止，不能重建活句柄、授予 GUI 控制、发布 checkpoint 或独立释放数据库槽位。worker close、EOF、Pod 删除都不构成充分证据。

## 组件验证

`//crates/browser:browser_contracts_test` 及 Kubernetes/node 单测覆盖恶意回调、授权/顺序错配、不确定确认、manifest 篡改、profile 挂载和不同停止凭据类型。手动目标 `//crates/node:browser_live_test` 在临时 K3s 节点及[额外最小权限 RBAC](../../deploy/testing/browser-component-rbac.yaml)下，执行真实 init、启用 sandbox 的 Chromium、私有 attach、节点保护和物理停止。回调 policy 明确为测试夹具，不代表生产数据库授权或 App ready。

夹具只在取得并保持活进程停止证明后，执行带 UID 前置条件的立即 API 清理，避免 kubelet 的滞后资源账目拒绝下一个 3 CPU 测试 Pod。这不是生产强制删除路径。期限场景保持空闲管道直到原 watchdog 截止时间，避免 EOF 自然关闭掩盖到期终止。

[绑定源码的验证记录](../evidence/browser-runtime-2026-10-11.json)列出测试结果、实际运行回读和适用范围。
