# 迁移基线

- 分支：`ui/slint-kit`
- 基线提交：`667ee4e`（重构 call-ui ICE/发送器大函数）
- Windows 构建：`cargo build -p media_talk_rust --bin media_talk_rust -j 1`
- Windows 运行：`target/debug/media_talk_rust.exe gui`
- 设备构建：`cargo build --release -p media_talk_rust --features hw-decode,kms`
- 设备运行：`SLINT_BACKEND=linuxkms-software`（root，KMS noseat）
- 迁移前测试：`cargo test -p call-ui --lib`，63 passed

## 工作区注意事项

迁移开始时工作区已有未提交改动，包括休眠时间功能、熄屏/接线、打包脚本和文档。
本次只修改 UI 迁移相关文件，不提交无关工作。

## 已知问题

- 主页时钟卡片在大字号下可能溢出（迁移中已缩小字号并调整 4:16 比例）。
- 设置页初始滚动位置在早期截图里不稳定，复测后已从顶部开始。
- KitSelect 在 Windows 上弹层正常；KMS 设备尚未实机验证。
