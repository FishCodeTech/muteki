"""Muteki 可扩展 Agent 平台层。

包含平台契约（``muteki.platform.contracts``，CORE-01）、PlatformStore /
投影 / outbox / reconciler（CORE-02）、DomainModule 与 workspace kind
注册表（``registry``、``builtin_modules``、``registry_api``，CORE-03）以及
Command API 与内置命令 Handler（``command_api``、``command_handlers``，
COMMAND-01）。Capability Gateway 等实现由后续工作包落地。
"""
