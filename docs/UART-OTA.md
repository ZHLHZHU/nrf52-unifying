# 通过 RP2040 SWD 更新 nRF52840

电脑 → RP2040 SWD 邮箱 → 板内 UART → nRF 暂存区 → CRC 校验 → bootloader 更新。

## 当前状态

- nRF 接收端已刷入并回读校验，RX=P0.06、TX=P0.08。
- RP 转发固件已刷入，回读校验通过；DAPLink 当前接 RP。
- UART OTA 6 项测试、现有 UART parser 10 项测试通过。
- 上传脚本通过镜像检查，拒绝 bootloader 镜像。
- 实板 UART 握手、46,760 字节传输、nRF 回读 CRC、重启和 BOOT 状态确认全部通过。
- 实测版本从 `1791650792` 更新至 `1791651163`。

## 接手者先看这里

当前链路是：电脑 → DAPLink → RP2040 SWD → 板内 UART → nRF。
不需要连接 nRF USB，也不需要把 DAPLink 切到 nRF。这里的 UART OTA 通过有线 UART 传输，不是无线升级。

1. 保持整板 USB-C 供电；DAPLink 接 RP 的 SWDIO/SWCLK/GND，电源输出断开。
2. SSH 登录 `lh@lh-pc-vm`，确认没有其他 OpenOCD / probe-rs 会话占用探针。
3. 修改、构建 nRF 应用，再调用下面的上传脚本。

```sh
ssh lh@lh-pc-vm
cd /home/lh/nrf52-unifying
cargo build --locked -p nrf-demo-app --release
python3 /home/lh/unifying-kvm/rp2040/nrf_uart_update.py \
  --rp-elf /home/lh/unifying-kvm/rp2040/build/rp2040_kvm.elf \
  --image /home/lh/nrf52-unifying/target/thumbv7em-none-eabihf/release/nrf-demo-app
```

这里的 `--rp-elf` 用于查找 RP RAM 邮箱地址，必须对应板上已烧录的 RP 固件。仅修改 nRF 时，不需要重新烧 RP。
若重新构建或修改了 RP，先按下面第 1 步将匹配 ELF 烧到板上。

脚本自动启动 OpenOCD、独占 RP 转发模式、检查 nRF 能力、上传、CRC 校验和验证重启后的版本。
成功输出形如 `SUCCESS: VER unifying/1 build=1791651163 boot state confirmed`。
只有 `REBOOT` 或 `Image verified` 还不能算验证完成，应等到 `SUCCESS`。
结束时脚本复位 RP，恢复 KVM；更新期间键盘转发暂停。

排错：

- 找不到探针：检查 DAPLink 是否透传给虚拟机，`lsusb` 应出现 `0d28:0204`。
- 邮箱版本错误：核对 `--rp-elf` 是否匹配板上固件，必要时重刷 RP。
- UART 超时：确认整板供电，以及 GP4 → P0.06 RX、GP5 ← P0.08 TX、GND 共地。
- 尚未提交时中断上传：旧应用保留，nRF 会话无请求 60 秒后退出；再完整上传。
- 已提交却无法确认启动：保留供电并诊断；必要时参考 BOARD-KVM.md 的 nRF SWD 恢复方式，避免整片擦除配对区。

## 后续使用

保持 Type-C 为整板供电，DAPLink 的 SWDIO/SWCLK/GND 接 RP2040，不并联调试器供电输出。RP 转发版已安装，后续直接执行第 3 步。

下面命令在 `lh@lh-pc-vm` 上执行。

### 1. 刷 RP 转发版

```sh
openocd -f interface/cmsis-dap.cfg -f target/rp2040.cfg \
  -c 'adapter speed 1000' \
  -c 'program /home/lh/unifying-kvm/rp2040/build/rp2040_kvm.elf verify reset exit'
```

### 2. 先检查 UART

```sh
python3 /home/lh/unifying-kvm/rp2040/nrf_uart_update.py \
  --rp-elf /home/lh/unifying-kvm/rp2040/build/rp2040_kvm.elf
```

预期输出真实 `VER unifying/1 build=...` 和 `OTA UART1 BLOCK64 CRC32`。
超时表示尚未连通，不能算升级成功。

### 3. 上传新 nRF 应用

```sh
python3 /home/lh/unifying-kvm/rp2040/nrf_uart_update.py \
  --rp-elf /home/lh/unifying-kvm/rp2040/build/rp2040_kvm.elf \
  --image /home/lh/nrf52-unifying/target/thumbv7em-none-eabihf/release/nrf-demo-app
```

脚本从 ELF 提取 0x7000 起的应用，不传 ELF 文件头，也不更新 bootloader。
固件需保留 UART OTA 功能；没有对应能力标记的镜像会被拒绝。
只有完整上传、nRF 回读 CRC 正确、重启后的版本匹配且状态为 BOOT，才输出 SUCCESS。

## 更新行为

进入上传模式时 RP 暂停 KVM；脚本结束后复位 RP，恢复正常工作。
nRF 每块接收 64 字节，验证偏移和长度；重复块必须与已写入内容一致。
数据先写 DFU 区，校验通过后才提交，配对区不写入。
传输超过60秒没有新请求会退出更新会话，旧应用仍保留。
CRC用于发现传输损坏，不提供数字签名认证。

## 文件

固件源码位于虚拟机的两个项目中；本目录保存上传脚本、协议模块、RP转发模块、固件快照。
`rp2040_kvm.elf` 对应新的邮箱布局，不能搭配旧RGB或旧KVM固件读取邮箱。
