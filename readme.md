# 这是某嵌入式厂商Linux设备的rust版本的media程序
- 运行设备是`Linux radxa-cm3-rpi-cm4-io 5.10.160-19-rk356x #eeb393dfb SMP Fri Oct 13 04:13:28 UTC 2023 aarch64 GNU/Linux`

## 需要实现的功能
1. [ ] 搜索网络摄像头, 并拿到音视频推流数据. 会使用到 v4l2, alsa, rtsp, onvif
   - [ ] 通过硬件解码
   - [ ] 显示到web


## 环境配置
1. 常规安装rust.
2. 配置交叉编译环境: 
   - 安装编译工具:`rustup target add aarch64-unknown-linux-gnu`
   - 安装zig实现轻量级交叉编译: `choco install zig` `cargo install cargo-zigbuild`
   - 编译命令:`cargo zigbuild --target aarch64-unknown-linux-gnu.2.31 --release`
3. windows开启smb, 板子挂载
    - 对需要的文件夹开启共享, 用户使用`Everyone`, 勾选完全控制权限, 可能还会需要在文件夹属性选项卡的安全选项配置, 创建`Everyone`用的, 避免被拦截.
    - 板子输入`sudo mount -t cifs //192.168.1.17/share ~/win_share -o username=账号,password=密码,iocharset=utf8,uid=radxa,gid=radxa,rw`, 账号通过`whoami`查看.
4. `OpenSpec`配置
   - 安装`npm install -g @fission-ai/openspec@latest`
   - 初始化项目`openspec init --tools claude`

## `Openspec` 使用方法
1. `/opsx:explore`探索项目以及梳理项目框架
2. `/opsx:propose`实现某个功能, 这时会生成一堆md文件, 主要检擦`proposal.md/task.md`, 看看对任务描述是否正确, 不对的话在ai内对话修改
3. `openspec validate 项目名称 --strict`检查任务清单, 判断生成内容是否100%有效, `strict`限制每个功能描述必须准确, 不准出现语义模糊
4. `/openspec:apply 项目名称`核对任务清单无误后用这个开始写代码, 项目名称是: ./openspec/changes/内的其中一个
5. `/openspec:archive 项目名称`归档项目, 避免切换终端后记忆丢失
6. `/openspec:onboard`老项目用这个生成项目规范
