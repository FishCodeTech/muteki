# M6 自建平台验收部署

本目录保存 M6 自建平台环境的可复现部署清单。清单只包含固定版本、容器名、端口、
网络和非敏感配置模板。账户、密码、Token、Cookie、浏览器 storage state 和实际
`appsettings.json` 由 `remote-bootstrap.sh` 在远端私有目录生成，不进入仓库。

## 固定版本

| 平台 | 上游版本 | 提交 | ARM64 镜像清单摘要 |
|---|---|---|---|
| CTFd | `3.8.7` | `ba53a21e53d1580f75e5186221563e030b839434` | `sha256:284f1f06c5464108c4eaaea8a28934cb1e81d491e3f0fe60f3d686cf38593e41` |
| rCTF | `v2.1.3` | `c54549ecffc9b4275b4d2163444d2601a0dd2147` | `sha256:93093d1efc38919732260bf06ae79b4d85ea3e3c254353e58f899132183a4605` |
| GZCTF | `v1.8.7` | `932b0d3a7e98e79dac88cede6657d17ab271b5d1` | `sha256:fcf87ae5b750daa9d1ecbe65c3a7ffdfd7cad99871570f54b589438151f4a19e` |

依赖镜像也在 Compose 文件中按摘要固定：MariaDB `10.11`、Redis `4`、
PostgreSQL `18.0` 和 Redis `8.2.2`。GZCTF 动态题使用
`traefik/whoami:v1.11.0`；验收主机的 ARM64 镜像摘要为
`sha256:c2c89736959e8830c3db090e2f54500499ceb0460267f91dce494728d7327770`。

## 隔离范围

- 远端根目录：`/home/snowywar/muteki-m6-20260823`
- CTFd：远端回环端口 `34101`，容器和网络前缀 `muteki-m6-ctfd`
- rCTF：远端回环端口 `34102`，容器和网络前缀 `muteki-m6-rctf`
- GZCTF：远端回环端口 `34103`，容器和网络前缀 `muteki-m6-gzctf`
- Muteki 后端和前端使用本机独立目录、数据库和端口，不读取这些平台的持久卷。

所有平台端口只监听远端 `127.0.0.1`。本机验收通过独立 SSH 转发访问，避免把测试
平台暴露到其他网络。

## 部署

将本目录复制到远端隔离根目录后执行：

```bash
chmod 700 remote-bootstrap.sh
./remote-bootstrap.sh /home/snowywar/muteki-m6-20260823
docker compose -f ctfd/compose.yml --env-file ctfd/.env up -d
docker compose -f rctf/compose.yml --env-file rctf/.env up -d
docker compose -f gzctf/compose.yml --env-file gzctf/.env up -d
```

`remote-bootstrap.sh` 可重复执行；已存在的 Secret 文件不会被覆盖。
