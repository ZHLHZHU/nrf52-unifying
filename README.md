# nRF52840 优联键盘发射器

用 nRF52840 的**片上原生 2.4GHz 射频**(ESB 模式)模拟一个 Logitech Unifying 无线键盘,通过 USB-CDC 串口接收上位机命令向接收器发送按键,无需外接 nRF24L01 模块。

基于 [`rust-unifying`](../rust-unifying) 协议库与 embassy + embassy-boot 脚手架实现,保留 CDC OTA 升级能力(首刷之后纯靠 USB 线升级,不依赖 SWD)。

## 特性

- **片上射频**:nRF52840 RADIO 配置成 nRF24L01+ 兼容 ESB,直接和优联接收器通信
- **完整键盘**:配对、文本输入、原始 HID(功能键 / Ctrl·Alt·Shift 组合键)
- **链路保活**:连接态下后台持续 keep-alive,维持跳频同步,长间隔不丢键
- **配对持久化**:地址 / AES 密钥 / 防重放计数存入 flash,断电、重启、OTA 后免重配
- **CDC 操作**:配对与发送全部走串口命令;固件 OTA 也走同一根 USB 线

## 快速上手

```bash
# 确认固件在线
python3 tools/unifying.py ver

# 接收器侧打开配对窗口
sudo ltunify pair 60 &
# 配对(只需一次,之后持久化)
python3 tools/unifying.py pair

# 连接并发送按键
python3 tools/unifying.py connect
python3 tools/unifying.py type "hello world"
python3 tools/unifying.py key ctrl+alt+del
```

## 文档

- [使用文档](docs/USAGE.md) — 安装、配对、发送按键、OTA 升级、故障排查
- [技术文档](docs/TECHNICAL.md) — 架构、ESB 驱动、协议库、持久化、分区布局
- [迁移记](docs/MIGRATION.md) — 迁移过程的探索与经历(分享向)

## 工程结构

```
app/                  # 应用固件(运行在 ACTIVE 分区)
  src/
    main.rs           # USB-CDC、命令分发、保活循环
    esb_radio.rs      # UnifyingRadio 在片上 RADIO 上的实现
    unifying_hal.rs   # Clock + AesEncryptor
    keymap.rs         # ASCII → HID 扫描码
    storage.rs        # 配对持久化(STORAGE 分区)
bootloader/           # embassy-boot bootloader
tools/
  unifying.py         # 上位机操作 CLI
  cdc_dfu.py          # Linux OTA 升级脚本
  cdc_dfu.ps1         # Windows OTA 升级脚本
docs/
```

> 依赖 `rust-unifying` 协议库(path 依赖,需与本仓库同级放置:`../rust-unifying`)。

## 构建与烧录

```bash
# 编译
cargo build -p nrf-demo-app --release

# 导出 OTA 镜像
llvm-objcopy -O binary \
  target/thumbv7em-none-eabihf/release/nrf-demo-app app.bin

# OTA 升级(首刷需 SWD,详见 docs/USAGE.md)
python3 tools/cdc_dfu.py --port /dev/ttyACM0 --image app.bin
```
