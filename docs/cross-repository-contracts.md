# 跨仓库契约与发布边界

本项目保留四个独立 Git 仓库。拆分是有意的：主程序、Android 客户端、OCR runtime 和资源快照使用不同的技术栈、发布节奏和 GitHub Release 产物。

## 仓库职责

| 仓库 | 负责内容 | 发布产物 |
| --- | --- | --- |
| `wuwa-gacha-tool` | React/Tauri 桌面端、共享数据库、同步和消费端逻辑 | 桌面安装包与更新清单 |
| `wuwa-gacha-tool-android` | Android UI、Room 数据库、同步和移动端导入 | Android APK |
| `wuwa-gacha-tool-ocr-runtime` | OCR 脚本、模型和 sidecar | OCR 压缩包与 `ocr-component.json` |
| `wuwa-gacha-tool-resources` | 角色/武器目录、图标和立绘快照 | 资源压缩包与 `resource-manifest.json` |

OCR 和资源必须保持独立 Release。它们可以在没有主程序版本更新时发布，也不应与桌面安装包或 Android APK 共用一个 GitHub Release。

## 当前跨仓库契约

### OneDrive `sync/v1`

桌面端和 Android 端共享 `Wuwa Gacha Tool/gacha-data.db` SQLite 快照。快照包含抽卡记录、导入状态和卡池历史边界；设备路径、资源缓存、同步基线和凭据不上传。上传前生成一致性快照，下载后检查大小、SQLite 完整性、schema 和必要表，再事务性替换本地共享数据。

两端都必须使用兼容的数据库 schema 和迁移。两端从共同 ETag/hash 基线同时发生修改时，必须停止同步，不能采用最后写入覆盖。

规范文档：

- 桌面端：[docs/cloud-sync-v1.md](cloud-sync-v1.md)
- Android：[Android cloud-sync-v1.md](https://github.com/juliy819/wuwa-gacha-tool-android/blob/main/docs/cloud-sync-v1.md)

### Resource manifest

资源仓库生产 `resource-manifest.json`、`catalog.json`、`icons/` 和 `portraits/`。桌面端与 Android 都必须校验 manifest、SHA-256、安全路径和资源映射；资源目录或字段变化时，先验证两个消费端再发布。

### OCR component manifest

OCR runtime 生产 `ocr-component.json` 及平台压缩包。桌面端只下载、校验和调用已发布 sidecar，不在主程序仓库构建 OCR runtime。OCR runtime 读取主程序安装的资源包，因此资源目录或 catalog 字段变化时也要运行 OCR 资源兼容性验证。

## 变更联动规则

1. 只改单仓库内部实现时，在对应仓库完成测试和发布。
2. 修改 `sync/v1`、数据库 schema、资源 manifest 或 OCR manifest 时，必须同时检查生产端和所有消费端，并在提交或 PR 中列出验证仓库。
3. 跨仓库发布按“生产端先发布、消费端兼容后发布”的顺序执行；消费端必须保留最后一个有效资源/OCR版本作为失败回退。
4. 不为了减少仓库数量复制 OCR 模型、资源快照或另一端的完整业务实现。

## 推荐验证矩阵

| 变更 | 至少验证 |
| --- | --- |
| SQLite schema / `sync/v1` | 桌面端 Rust 同步测试、Android 单元测试、两端 schema/迁移和手工双端快照检查 |
| Resource manifest/catalog | resources 本地构建解包、桌面端资源安装测试、Android 资源安装测试、OCR 资源样例回归 |
| OCR component manifest/sidecar | OCR `--self-check` 和请求样例、桌面端 manifest 下载/校验测试 |
| 单端 UI 或业务逻辑 | 对应仓库的类型检查、单元测试和构建 |
