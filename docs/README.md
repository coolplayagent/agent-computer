# 文档 / Documentation

## 01. 中文

1. [项目概览](zh-CN/01-overview.md)
2. [开发与构建](zh-CN/02-development.md)
3. [架构与契约](zh-CN/03-architecture.md)
4. [渐进式实现计划](zh-CN/04-implementation-plan.md)
5. [验证与交付证据](zh-CN/05-verification.md)
6. [领域状态契约](zh-CN/06-domain-contracts.md)
7. [ComputerSet 声明验证](zh-CN/07-declarations.md)
8. [PostgreSQL 声明持久化](zh-CN/08-persistence.md)
9. [控制服务与服务凭据](zh-CN/09-control-service.md)
10. [声明计划与原子 apply](zh-CN/10-plans-and-apply.md)
11. [持久化协调与租约](zh-CN/11-reconciliation-coordination.md)
12. [受限 Kubernetes 适配器](zh-CN/12-kubernetes-adapter.md)
13. [持久化 JuiceFS 卷供应](zh-CN/13-volume-provisioning.md)
14. [Candidate 存储准备](zh-CN/14-candidate-storage.md)
15. [资源运行授权](zh-CN/15-runtime-authorization.md)
16. [Computer 持久化启动准入](zh-CN/16-start-admission.md)
17. [Candidate 持久化准备 worker](zh-CN/17-candidate-preparation-worker.md)
18. [持久化逻辑连接会话](zh-CN/18-connection-sessions.md)
19. [Candidate 持久化写入租约](zh-CN/19-candidate-writer-leases.md)
20. [Candidate 有界文件保存](zh-CN/20-bounded-file-saves.md)
21. [Candidate 文件 HTTP 网关](zh-CN/21-file-http-gateway.md)
22. [Sandbox 进程监督器](zh-CN/22-sandbox-supervisor.md)
23. [持久化执行准入](zh-CN/23-execution-admission.md)
24. [持久化执行派发日志](zh-CN/24-execution-dispatch.md)

## 02. English

1. [Overview](en/01-overview.md)
2. [Development and builds](en/02-development.md)
3. [Architecture and contracts](en/03-architecture.md)
4. [Incremental implementation plan](en/04-implementation-plan.md)
5. [Verification and delivery evidence](en/05-verification.md)
6. [Domain state contracts](en/06-domain-contracts.md)
7. [ComputerSet declaration validation](en/07-declarations.md)
8. [PostgreSQL declaration persistence](en/08-persistence.md)
9. [Control service and service credentials](en/09-control-service.md)
10. [Definition plans and atomic apply](en/10-plans-and-apply.md)
11. [Durable reconciliation coordination](en/11-reconciliation-coordination.md)
12. [Constrained Kubernetes adapter](en/12-kubernetes-adapter.md)
13. [Durable JuiceFS volume provisioning](en/13-volume-provisioning.md)
14. [Candidate storage preparation](en/14-candidate-storage.md)
15. [Resource runtime authorization](en/15-runtime-authorization.md)
16. [Durable Computer start admission](en/16-start-admission.md)
17. [Durable Candidate preparation worker](en/17-candidate-preparation-worker.md)
18. [Durable logical connection sessions](en/18-connection-sessions.md)
19. [Durable Candidate writer leases](en/19-candidate-writer-leases.md)
20. [Bounded Candidate file saves](en/20-bounded-file-saves.md)
21. [Candidate file HTTP gateway](en/21-file-http-gateway.md)
22. [Sandbox process supervisor](en/22-sandbox-supervisor.md)
23. [Durable execution admission](en/23-execution-admission.md)
24. [Durable execution dispatch journal](en/24-execution-dispatch.md)

两组文档使用相同序号和主题，随实现同步更新。详细设计和需求追踪继续保留在 `codespec/`，知识导航由 `relay-knowledge` 管理。

Both language groups use matching numbers and topics and are updated with the implementation. Detailed specifications remain in `codespec/`; `relay-knowledge` manages the knowledge navigation maps.
