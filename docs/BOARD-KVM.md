# 当前一体板硬件与调试记录

更新：2026-10-11。本文以已经焊接、上电和联调的实板为准。

## 仓库与连接

| 项目 | 位置 / 链接 |
| --- | --- |
| Linux 虚拟机 | `ssh lh@lh-pc-vm`（用户名 lh；从开发 Mac 使用现有 SSH 配置） |
| RP / KVM 仓库 | `/home/lh/unifying-kvm`；https://github.com/ZHLHZHU/unifying-kvm |
| nRF 仓库 | `/home/lh/nrf52-unifying`；https://github.com/ZHLHZHU/nrf52-unifying |
| Pico SDK | `/home/lh/pico-sdk`，2.2.0，`a1438dff1d38bd9c65dbd693f0e5db4b9ae91779` |
| TinyUSB | SDK 子模块 `86ad6e56c1700e85f1c5678607a762cfe3aa2f47` |
| EDA 项目（Mac） | `/Users/lh/Documents/LCEDA-Pro/projects/unifying-kvm` |
| 模组资料 | https://bluetoothdx.com/nicenano.html |
| 模组说明书 | https://daxiongble.oss-cn-shenzhen.aliyuncs.com/zmk/nice%EF%BC%81nano%E4%BA%A7%E5%93%81%E8%AF%B4%E6%98%8E%E4%B9%A6.pdf |

## 实板配置

- 四层板：L2 为 GND，L3 用于电源；nRF 天线区各层遵守禁铜区。
- USB-C 输入 5V，经 F1 到 `5V_SYS`；RT9013 产生 RP 所需 3.3V。
- RP2040 内置线性稳压器产生 1.1V 内核供电，非 DC-DC，不需要电感。
- U3 是 W25Q128JVSIQ，SOIC-8、128Mbit（16MiB）；构建设置 `PICO_FLASH_SIZE_BYTES=16777216`。
- X1 为 5032 两脚无源 12MHz 晶振；普通电阻、电容使用 0603，较大容量 MLCC 按实际封装。
- USB-A 是 RP2040 原生 USB Host。TPS2553 控制 `HOST_5V`；GP14 高电平使能，GP15 为低有效开漏故障输入。
- 设计限流范围约 0.8–1.5A；曾讨论 24k / 27k ILIM，最终实装阻值尚未核对。
- nice!nano 兼容 nRF52840 模组用邮票孔直接焊接。板内 `NRF_PWR` 经 R12 接模组 VBUS（5V 输入）；不能把模组 VBUS 当成 3.3V IO 电源。
- nRF USB 接口被电容挡住；目前通过 RP SWD 转发 UART 更新 nRF。
- 独立 UART 调试排针已删除；保留 RP 与 nRF 的 SWD。

## 实际信号连接

| 功能 | RP 引脚 / 网络 | nRF 模组引脚 |
| --- | --- | --- |
| RP 发送，nRF 接收 | GP4 / UART1 TX → `NRF_RXD` | `006` / P0.06 RX，模组焊盘 2 |
| nRF 发送，RP 接收 | GP5 / UART1 RX ← `NRF_TXD` | `008` / P0.08 TX，模组焊盘 3 |
| 公共地 | GND | GND |
| UART 参数 | 1,000,000 baud，8N1，无流控，RX 内部上拉 | UARTE0 |
| USB Host 使能 | GP14 / `USB_EN` | 不连接 nRF |
| USB Host 故障 | GP15 / `USB_FAULT_N`，内部上拉 | 不连接 nRF |
| RGB | GP16 / GP17 / GP18，共阳、低有效 | — |

**002 / P0.02 在当前 PCB 未连接。旧原型的 P0.02 RX、P0.06 TX 配置不适用于本板。**

## RGB LED1 接线与驱动

LED1 型号 SZYY0603RGB-B，为普通四脚共阳 RGB，不是 WS2812，不使用串行数据协议。

| 颜色 | RP2040 GPIO（芯片脚） | 串联电阻（原理图值） | LED1 阴极脚 |
| --- | --- | --- | --- |
| 红 | GP16（27）/ LED_R | R9，1kΩ | 1 / LED_R_K |
| 绿 | GP17（28）/ LED_G | R10，1kΩ | 2 / LED_G_K |
| 蓝 | GP18（29）/ LED_B | R11，1kΩ | 3 / LED_B_K |

LED1 的 4 脚为公共阳极，接 3V3。电流方向：3V3 → LED → 电阻 → RP GPIO。
GPIO 输出低电平点亮对应通道，高电平熄灭；PWM 控制时占空比也要按低有效处理。
初始化先 gpio_put(pin, 1)，再设置输出，避免初始化闪灯。三个 GPIO 均为高时全灭。
例：红色为 GP16=0、GP17=1、GP18=1；绿色为 1/0/1；蓝色为 1/1/0。
原理图阻值为 1kΩ，实际焊装阻值未单独测量。

KVM 固件用这颗 RGB 显示目标状态：slot 0 绿、1 蓝、2 红；连不上快闪；配对慢闪；故障白色。键盘 Scroll Lock 灯不再使用。nRF 模组自己的 LED 是独立的。

## SWD 接线与供电

当前只有一个 Air32 CMSIS-DAP，最后连接在 RP2040。

| 调试器信号 | RP 调试时 | nRF 调试时 |
| --- | --- | --- |
| SWDIO | RP SWDIO | 模组 DIO / SWD |
| SWCLK | RP SWCLK | 模组 CLK / SWC |
| GND | 板 GND | 板 GND |
| 电源输出 | 断开，由板 USB-C 供电 | 断开，由板 USB-C 供电 |

SWD IO 是 3.3V 电平。有单独 VTref 输入的调试器应接目标 3.3V；不能把调试器电源输出误当 VTref。
可以同时将 Type-C 和调试器 USB 插入电脑，但不并联调试器供电输出。

- 探针 VID:PID：`0d28:0204`，Air32 CMSIS-DAPv2。
- 序列号：`000100011a5a5bb60000000000074c2ea5a5a5a597969908`。
- 已用 OpenOCD 0.12.0、SWD 1000kHz 成功烧录和回读验证两颗芯片。
- 多探针时用 `-c 'cmsis_dap_serial <序列号>'` 选择。

RP 构建与烧录（在虚拟机执行）：

```sh
cd /home/lh/unifying-kvm
cmake -S rp2040 -B rp2040/build -DPICO_BOARD=pico -DPICO_SDK_PATH=/home/lh/pico-sdk -DPICO_FLASH_SIZE_BYTES=16777216 -DRP_UART_SMOKE=OFF
cmake --build rp2040/build --target rp2040_kvm
openocd -f interface/cmsis-dap.cfg -f target/rp2040.cfg -c 'adapter speed 1000' -c 'program /home/lh/unifying-kvm/rp2040/build/rp2040_kvm.elf verify reset exit'
```

nRF 应用位于 `target/thumbv7em-none-eabihf/release/nrf-demo-app`，这是无扩展名 ELF。
需要 SWD 恢复应用时，先提取真实应用段，避免 ELF 的地址 0 文件头覆盖 bootloader：

```sh
cd /home/lh/nrf52-unifying
cargo build --locked -p nrf-demo-app --release
arm-none-eabi-objcopy -O ihex --only-section=.vector_table --only-section=.text --only-section=.rodata --only-section=.data target/thumbv7em-none-eabihf/release/nrf-demo-app /tmp/nrf-app.hex
openocd -f interface/cmsis-dap.cfg -f target/nrf52.cfg -c 'adapter speed 1000' -c 'program /tmp/nrf-app.hex verify reset exit'
```

不要直接用 OpenOCD `program` 烧上述 nRF ELF，也不要整片擦除已有配对数据。

nRF 分区：bootloader `0x00000–0x05FFF`，boot state `0x06000–0x06FFF`，app `0x07000–0x7EFFF`，DFU `0x7F000–0xF7FFF`，配对存储 `0xF8000–0xFFFFF`。
后续应用升级优先使用 [UART OTA 指南](UART-OTA.md)，DAPLink 留在 RP 即可。

## 已验证与下一步

2026-10-11 实测：

- 空载电流约 11mA；3.3V、1.1V 电源正常（用户万用表测量）。
- RP SWD 识别及 16MiB Flash 烧录/回读通过；RGB 闪灯测试通过。
- nRF SWD、修正后的 UART 双向握手通过。
- 经 RP 转发 UART 上传 46,760 字节，nRF 回读 CRC、重启、新版本及 BOOT 状态全部确认；版本 `1791650792` → `1791651163`。
- nRF slot 0：`PAIRED=1 CONN=1`；RP 开机释放全部按键命令获得成功回复。
- slot 1：`PAIRED=0`，选择它返回 `SWITCHED 1 EMPTY`。
- RP 最新状态 `WAIT_RELEASE`（状态码 12）、fault=0；键盘已枚举，接口 0 为 Boot 键盘，正在等待有效的全释放输入报告。

2026-10-11 本机 SWD 联调补充（探针接 RP2040，整板由 USB-C 供电）：

- 经板内 UART 中继读到 nRF `build=1791651163`、`STATUS SLOT=0 PAIRED=1 CONN=1`，计数随命令递增。
- UART 注入的 `UKEYDOWN 00 04` / `UKEYDOWN 00` 与 `UKEYDOWN 00 05` / `UKEYDOWN 00` 均返回 `OK`，`CNT` 每次按下+释放增加 2（5640→5642、5643→5645）。尚无目的端输入记录；用户报告接收器未插电脑，与 ACK 现象存在待解释的矛盾，不能据此确认目的端身份或完整键盘路径。
- 切槽往返：`USWITCH 01` → `SWITCHED 1 EMPTY`（`SLOT=1 PAIRED=0`），`USWITCH 00` → `SWITCHED 0 CONNECTED` 并恢复 `PAIRED=1 CONN=1`，计数未倒退，配对持久化正常。
- RP `reset run` 后 nRF 仍 `PAIRED=1 CONN=1`，KVM 自然回到 `state=12`、`fault=0`，无 UART 超时。
- 板载 USB-A 已枚举键盘：设备地址 1，接口 0 的 IN 端点为 `0x81`、8 字节，Boot SET_PROTOCOL 成功，接收已提交。`SIE_STATUS` 速度位为 9:8，实读为全速；`SOF_RD` 在 `USB_BASE+0x48`，帧计数变化。此前使用错误位号和寄存器偏移得出的硬件故障结论已撤回，详见 KVM 仓库 `docs/HARDWARE.md`。
- 用户确认普通有线 iKBC 并持续按住右箭头。排查两个 HID 接口、Boot/Report 模式、16ms idle、无 KVM 解码/LED 的 UART-smoke 对照，均未取得中断输入；控制查询确认配置值 1，接口 0 实际返回 8 字节全 0。主机 IRQ 未记录 RX timeout 或 DATA_SEQ_ERROR。总线复位及缓冲同步时序实验未恢复输入。临时固件及 SDK 改动均已撤除，两种固件重新构建，板上已恢复并校验正式版，仍 `WAIT_RELEASE`、fault=0。根因未确认，需 USB 报文或正常主机输入对照；完整记录见 KVM 仓库 `docs/HARDWARE.md`。
- 后续 HCD 采样取得线上证据：344 个端点 `0x81` 的 IN token，后面都是 CRC 正确的 DATA1 全零报告，主机始终不回 ACK。根因是 RP2040 自动轮询端点的芯片缺陷（pico-feedback #394）。现由 KVM 仓库的 `rp2040/native/hcd_epx.c` 让全部端点改走 EPX，并去掉了会引起反复重枚举的 `RESET_BUS`。实板上键盘稳定挂载，KVM 进入 READY、fault=0，60 秒内收到 29 份真实按键报告。详见 KVM 仓库 `docs/HARDWARE.md`。

- 端到端：用户确认在键盘上输入后，目的电脑同步收到。

**当前可日常使用：** 三目标切换、长按配对（UART `UPAIR <slot>`）、RGB 状态、目标连不上时无限重试都已在实板上使用。未验证 USB 电气波形、阻抗及带载浪涌。完整开发历程见 KVM 仓库 [docs/JOURNEY.md](https://github.com/ZHLHZHU/unifying-kvm/blob/master/docs/JOURNEY.md)。

SWD 诊断变量地址随构建变化，应从当前 ELF 用 `arm-none-eabi-nm` 查询 `rp_debug_*`、`hid_dev`、`rp_ota_mailbox`，不要照抄旧地址。
