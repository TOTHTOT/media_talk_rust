# 这是某嵌入式厂商Linux设备的rust版本的media程序
- 运行设备是`Linux radxa-cm3-rpi-cm4-io 5.10.160-19-rk356x #eeb393dfb SMP Fri Oct 13 04:13:28 UTC 2023 aarch64 GNU/Linux`

## 环境配置
1. 常规安装rust.
2. 配置交叉编译环境: 
   - 安装编译工具:`rustup target add aarch64-unknown-linux-gnu`
   - 安装zig实现轻量级交叉编译: `choco install zig` `cargo install cargo-zigbuild`
   - 编译命令:`cargo zigbuild --target aarch64-unknown-linux-gnu.2.31 --release`
3. windows开启smb, 板子挂载
    - 对需要的文件夹开启共享, 用户使用`Everyone`, 勾选完全控制权限, 可能还会需要在文件夹属性选项卡的安全选项配置, 创建`Everyone`用的, 避免被拦截.
    - 板子输入`sudo mount -t cifs //192.168.1.17/share ~/win_share -o username=账号,password=密码,iocharset=utf8,uid=radxa,gid=radxa,rw`, 账号通过`whoami`查看.
