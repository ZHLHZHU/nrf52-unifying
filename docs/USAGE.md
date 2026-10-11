# 使用文档 —— nRF52840 优联键盘发射器

> 当前自制 KVM 板：RX=P0.06 / 006、TX=P0.08 / 008；P0.02 未连接。板 USB-C 供电，DAPLink 当前接 RP，nRF 经板内 UART 更新。实板接线与 SWD 操作见 [BOARD-KVM.md](BOARD-KVM.md)；下文原开发板 USB/引脚记录仅适用于历史配置。


本文说明如何使用 nRF52840 优联键盘发射器:从首次烧录、配对,到日常通过命令行发送按键,以及固件 OTA 升级。

设备通过一根 USB 线连接上位机,枚举为一个 CDC 串口(Linux 下通常是 `/dev/ttyACM0`)。所有操作都通过这个串口完成,不需要 SWD 调试器(首次烧录除外)。

---

## 1. 准备

### 硬件

- nRF52840 开发板(已刷入本项目固件),USB 连到上位机
- Logitech Unifying 接收器(USB dongle,型号 046d:c52b),插在目标电脑上
- 首次烧录还需要一个 SWD/DAPLink 调试器

### 上位机软件

```bash
# Python 串口库
pip install pyserial

# 配对时需要让接收器进入配对模式(Linux)
sudo apt install ltunify    # 或从源码编译
```

### 确认设备已连接

```bash
ls /dev/ttyACM*          # 应看到 /dev/ttyACM0
lsusb | grep 1209        # 应看到 1209:0001 nRF52840 Unifying CDC
```

若没有读写串口权限,把当前用户加入 `dialout` 组(`sudo usermod -aG dialout $USER` 后重新登录),或在命令前加 `sudo`。

---

## 2. 快速上手

典型流程:配对一次 → 之后连接 + 发按键。

```bash
cd nrf52-rs-scaffold

# 0. 确认固件在线
python3 tools/unifying.py ver
# -> VER unifying/1 build=...

# 1. 让接收器进入配对模式(在接收器所插的电脑上执行)
sudo ltunify pair 60 &

# 2. 配对(60 秒内执行)
python3 tools/unifying.py pair
# -> PAIRED 3D41950E0C CH=32

# 3. 连接
python3 tools/unifying.py connect
# -> CONNECTED CH=8

# 4. 发送按键
python3 tools/unifying.py type "hello world"
# -> TYPED 11/11
python3 tools/unifying.py key enter
# -> OK
```

配对信息会保存在设备 flash 里,**断电、重启、OTA 升级后都不会丢**,所以通常只需配对一次。之后每次使用直接 `connect` 即可。

---

## 3. CLI 命令详解

通用选项:

```bash
python3 tools/unifying.py [--port /dev/ttyACM0] [--debug] <命令> [参数]
```

- `--port`:串口路径,默认 `/dev/ttyACM0`
- `--debug`:打印底层收发的原始协议行,排错用

### ver — 查看固件版本

```bash
python3 tools/unifying.py ver
# VER unifying/1 build=1780592198
```

### pair — 配对

需要接收器先进入配对模式。配对是概率性的(接收器跳频),CLI 会自动重试。

```bash
# 接收器侧:打开 60 秒配对窗口
sudo ltunify pair 60 &

# 发射器侧:配对
python3 tools/unifying.py pair
# PAIRED 3D41950E0C CH=32   ← 接收器分配的地址 + 当前信道
```

成功后配对信息(地址、AES 密钥、信道、计数器)自动写入 flash。

### connect — 连接

用已保存的配对唤醒并连接接收器。连接后固件会在后台持续发保活包,维持链路。

```bash
python3 tools/unifying.py connect
# CONNECTED CH=8
```

### type — 输入文本

把文本逐字符作为键击发送。支持字母、数字、空格和常见符号。

```bash
python3 tools/unifying.py type "Hello, World!"
# TYPED 13/13      ← 成功键击数 / 总数
```

回复 `TYPED <ok>/<total>`:`ok` 是被接收器确认的键击数。链路正常时应为满分(如 `13/13`)。

> 注意:`type` 不能直接输入回车/换行(会与命令行结束符冲突)。需要回车请用 `key enter`。

### key — 发送按键 / 组合键

发送一帧原始 HID 报文,支持功能键、方向键、修饰键组合。

```bash
python3 tools/unifying.py key f5
python3 tools/unifying.py key enter
python3 tools/unifying.py key ctrl+alt+del
python3 tools/unifying.py key ctrl+shift+up
python3 tools/unifying.py key 0x4C          # 直接给 HID 键码(十六进制)
# 每条回复 OK 或 ERR SEND
```

**组合语法**:用 `+` 连接,修饰键 + 至多 6 个普通键。

支持的修饰键:

| 名称 | 说明 |
| --- | --- |
| `ctrl` / `lctrl` / `rctrl` | Control |
| `shift` / `lshift` / `rshift` | Shift |
| `alt` / `lalt` / `ralt` | Alt |
| `gui` / `win` / `meta` / `cmd` | GUI/Win/Cmd 键 |

支持的具名键(部分):

| 类别 | 键名 |
| --- | --- |
| 字母数字 | `a`–`z`、`0`–`9` |
| 编辑 | `enter` `esc` `backspace` `tab` `space` `delete` `insert` |
| 导航 | `up` `down` `left` `right` `home` `end` `pageup` `pagedown` |
| 功能键 | `f1`–`f12` |
| 其他 | `capslock` `printscreen` `scrolllock` `pause` |
| 符号 | `minus` `equal` `lbracket` `rbracket` `backslash` `semicolon` `quote` `grave` `comma` `period` `slash` |

也可以直接给十六进制 USB HID 键码,如 `0x4C`(Delete)。

### keydown — 发送按键(不自动释放)

发送一帧 HID 报文,**只发按下,不自动释放**。适用于 KVM 等程序化调用场景,由调用方负责状态管理和发送释放帧。

```bash
python3 tools/unifying.py raw "UKEYDOWN 00 04"    # 按下 'a'
python3 tools/unifying.py raw "UKEYDOWN 02 04"    # Shift + 'a'(= 'A')
python3 tools/unifying.py raw "UKEYDOWN 00"       # 释放所有键(发送空报文)
```

与 `UKEY` 的区别:
- `UKEY` 自动做 press + release 两帧(适合单次按键)
- `UKEYDOWN` 只发送当前帧的完整状态(适合状态式转发)

典型用法:KVM 转发器维护当前按下键集合,每次状态变化发一次 `UKEYDOWN`。这样多个键重叠按下/释放时不会因为 `UKEY` 的自动释放导致重复按键。

`UKEYDOWN` 每 128 帧自动持久化 AES counter,无需调用方额外处理。

### status — 查看状态

```bash
python3 tools/unifying.py status
# STATUS PAIRED=1 CONN=1 CH=62 CNT=26
```

- `PAIRED`:是否已配对(1=是)
- `CONN`:是否已连接
- `CH`:当前信道
- `CNT`:AES 计数器(每次键击递增)

### keepalive — 手动保活

发送一次保活包。一般不需要手动调用(连接后固件自动保活)。

```bash
python3 tools/unifying.py keepalive
# TICK
```

### delete — 删除配对

擦除 flash 中保存的配对信息。下次使用需重新 `pair`。

```bash
python3 tools/unifying.py delete
# DELETED
```

### raw — 发送原始协议行

直接发送一条底层协议命令,用于调试。

```bash
python3 tools/unifying.py raw USTATUS
python3 tools/unifying.py raw "UKEY 05 4C"     # Ctrl+Alt+Del
```

---

## 4. 固件 OTA 升级

更新固件无需 SWD,通过 CDC 串口完成。

```bash
# 1. 编译
cargo build -p nrf-demo-app --release

# 2. 导出原始二进制(注意:是 .bin,不是 elf/hex/uf2)
OBJCOPY=$(find ~/.rustup -name 'llvm-objcopy' | head -1)
"$OBJCOPY" -O binary \
  target/thumbv7em-none-eabihf/release/nrf-demo-app \
  target/thumbv7em-none-eabihf/release/nrf-demo-app.bin

# 3. OTA 刷入
python3 tools/cdc_dfu.py --port /dev/ttyACM0 \
  --image target/thumbv7em-none-eabihf/release/nrf-demo-app.bin
```

典型输出:

```
> INFO
< STATE BOOT BUILD=...
> WRITE 32128
< READY
> binary payload (32128 bytes)
< OK
> BOOT
< REBOOT
Update command sent. Device will reboot into the new firmware.
```

刷完设备会自动重启并重新枚举 USB,**等几秒**串口才会回来。升级不会清除配对信息。

> Windows 用户可用 `tools/cdc_dfu.ps1`(PowerShell)。

---

## 5. 首次烧录(仅一次,需 SWD)

全新的板子第一次需要用 SWD 烧入 bootloader 和 app,之后才能纯靠 OTA。

```bash
# 先烧 bootloader
probe-rs download --chip nRF52840_xxAA --speed 50 \
  target/thumbv7em-none-eabihf/release/nrf-demo-bootloader

# 再烧 app
probe-rs download --chip nRF52840_xxAA --speed 50 \
  target/thumbv7em-none-eabihf/release/nrf-demo-app
```

烧完板子会枚举出 USB-CDC 口,后续升级和操作都不再需要调试器。

---

## 6. 故障排查

### 找不到串口 / 打不开

- `ls /dev/ttyACM*` 确认设备存在
- 刚 OTA 完成时设备在重新枚举,等几秒再试
- 权限问题:加入 `dialout` 组或用 `sudo`

### 配对失败(ERR PAIR)

- 确认接收器侧已执行 `sudo ltunify pair 60` 且输出了 "Please turn your wireless device off and on"
- 配对是概率性的,多试几次
- 确认在配对窗口(60 秒)内执行了 `pair`

### type/key 返回 ERR SEND 或 TYPED 不满分

- 先 `connect` 重新建立链路再发
- 偶发单次失败是射频丢包,重发即可
- 确认接收器在发射器的射频范围内
- 长时间没操作后第一条命令偶尔会失败,固件会自动保活,正常情况下重连即可恢复

### 按键没出现在目标电脑上

- 确认接收器插在**目标电脑**上(不是发射器所在的机器)
- `status` 看 `CONN=1`、`CNT` 是否在递增
- 重新 `connect` 后再发

### 确认跑的是新固件

```bash
python3 tools/unifying.py ver
# 看 build= 后的时间戳是否变化
```

---

## 7. 命令速查

| CLI 命令 | 作用 |
| --- | --- |
| `ver` | 固件版本 |
| `pair` | 配对(需接收器在配对模式) |
| `connect` | 连接已配对接收器 |
| `type "文本"` | 输入文本 |
| `key <组合>` | 发送按键/组合键(如 `ctrl+alt+del`) |
| `status` | 查看状态 |
| `keepalive` | 手动保活 |
| `delete` | 删除配对 |
| `raw <行>` | 发送原始协议命令 |

底层串口协议(`raw` 或直接发串口):`VER` `UPAIR` `UCONNECT` `UTYPE <text>` `UKEY <mod> [keys]` `UKEEPALIVE` `USTATUS` `UDELETE`,以及 OTA 的 `PING` `INFO` `WRITE` `BOOT` `ABORT` `REBOOT`。

## 8. RP2040 UART 与维护模式

新增 UART 入口不改变 Unifying/ESB 算法、配对存储格式或原 CDC/OTA 线协议。无线状态现在由一个 worker 持有；UART 和 CDC 通过容量 8 的请求队列进入该 worker，各有独立容量 1 的回复队列。UART 回复入队时 worker 等待发送任务取走（每帧约 0.5ms），不再在队列满时丢弃回复并废弃后续请求。worker 不等待 CDC 输出，8ms 绝对保活截止时间在每条命令前检查，不依赖 CDC 连接或 DTR。原空闲 timeout 20/110 和 POFWARN 保存保留；POFWARN 是轮询，未测掉电余量。

UARTE0 使用 `BufferedUarte`（1 KiB RX 环形缓冲，硬件持续接收）。普通 `Uarte` 只在读 future 被轮询时接收，worker 阻塞发射频的约 100ms 内到达的后续行会丢失，RP 因此 UART 超时。

`UKEYDOWN` 按链路状态限时：`link_up`（最近一次发送或保活被接收器 ACK）时最多换 3 个信道尝试，搜索中只试 1 次立即回 `ERR SEND`，由后台 8ms 保活继续跳频。每条命令最坏约 0.1 秒回复，不再在一条命令里阻塞约 1.4 秒。

### 信号与帧

当前一体板：UARTE0 RX=P0.06（`006`），TX=P0.08（`008`），1,000,000 baud、8N1、无硬件流控（开发板时代为 RX=P0.02、TX=P0.06）。RP GP4 TX → nRF RX，RP GP5 RX ← nRF TX，两侧共地，使用 3.3V IO。正常与诊断模式均启用 RX 上拉，防止 RP reset 或断开 TX 时 RX 浮空。

```text
@00000001 VER<LF>
@00000002 USWITCH 00<LF>
@00000003 UKEYDOWN 00 04<LF>
@00000004 UKEYDOWN 00<LF>
```

这里 `<LF>` 表示真实字节 `0x0A`，不是可发送的尖括号文本。回复在原回复体前加同一编号，如 `@00000003 OK<CR><LF>`。编号是八位大写十六进制 u32；仅 UART 增加编号，CDC 保持原命令。

UART 允许 `VER`、`USTATUS`、`USWITCH <slot>`、`UKEYDOWN` 和 `UPAIR <slot>`（slot 必须显式给出，0–3），不允许删除；OTA 走单独的 `OTA` 前缀。`UPAIR` 失败时恢复该 slot 原有配对。不支持命令回复编号 `ERR CMD`，合法编号的非法参数回复 `ERR ARG`。每行上限 256 字节，支持拆包、粘包、LF/CRLF；非法编号、控制字节、超长或硬件损坏行丢弃到下一 LF，不将后缀重解释为命令。损坏时取消旧输入并请求 all-up；未接 UART 时不注入命令或模拟 ACK。

### CDC 独占

CDC DTR 建立后，worker 完成当前命令并向当前 slot 发 all-up，再授予 CDC 维护入口；所有 UART 请求（包括 VER）回复编号 `ERR BUSY`。OTA WRITE 在返回 READY 前暂停 tick，上传完成、错误、ABORT 或会话断开后恢复。二进制上传仍按原协议消费数据，不能把上传中的任意字节解释成文本 ABORT。

关闭 DTR、USB reset/deconfigure 或物理拔除 USB 都撤销维护，取消待发 CDC 回复及上传状态。锁定的 Embassy USB 0.6 在物理断开时不会替 CDC 清 DTR，因此固件另外观察 USB 生命周期，并要求重新配置后有新 DTR 请求；旧 DTR 不会让独立供电的板子永久 ERR BUSY。

已接受的 `BOOT`/`REBOOT` 在独立 future 中执行延迟 reset，不属于可取消的 CDC 会话。发送 `REBOOT` 回复后关闭 DTR 不应撤销重启；旧实现把 100ms timer 放在会话内部，会被工具关闭端口取消。是否成功升级必须以新固件启动/标记或实际重启为准，不能只看 `REBOOT` 回复。

RP 维护期间每秒 VER 探测；退出后执行 VER → USTATUS → 实际 slot all-up → USWITCH 该 slot → all-up，全部确认后等待物理键全释放再接受输入。FAULT 恢复不回放旧输入，也不反复自动 connect/扫频。

### 构建与验证边界

```sh
rustup target add thumbv7em-none-eabihf
cargo build --locked -p nrf-demo-bootloader -p nrf-demo-app --release
# UART parser 是纯 Rust，可独立用宿主工具链运行：
rustc --edition=2021 --test app/src/uart_link.rs -o /可写临时目录/nrf-uart-tests
/可写临时目录/nrf-uart-tests
```

两份 release ELF 已构建，10 项宿主 parser 测试通过（含 DMA 取消的 LF/残尾边界回归）；DTR/USB 生命周期修正经源码评审并重新构建。新板已由 Air32 识别为 nRF52840_xxAA、刷入两份 ELF 并回读校验；首次 CDC 返回 `VER unifying/1 build=1790942502`，slot 0 未配对（PAIRED=0、CONN=0），all-up 如实返回 `ERR NOTCONN`。

实际使用 `tools/cdc_dfu.py` 上传同一份 42800 字节 app，观察到 WRITE → READY → 二进制 → OK → BOOT → REBOOT；再次硬件 reset、USB 重新枚举后 INFO 返回 STATE BOOT。后续发现旧实现的延迟 reset 会被工具关闭 DTR 取消，因此该记录不能独立证明自动重启。修正已通过默认和诊断构建。reset 后立即打开旧串口可能遇到枚举竞态，应等新 CDC 端点出现再执行工具；VMware 可能需要重新连接设备到虚拟机。无线输入、独立供电和无 CDC 保活仍未验证。

**原厂 UF2 的 CDC 不是项目 OTA。** 首刷需将 Air32 接到新板 SWD/SWC/GND，nRF 由其 USB 供电；先识别 nRF52840 target，保存原厂引导器信息，再使用项目自己的地址布局：

```sh
probe-rs download --chip nRF52840_xxAA --probe 0d28:0204 --speed 50 \
  --non-interactive --verify target/thumbv7em-none-eabihf/release/nrf-demo-bootloader
probe-rs download --chip nRF52840_xxAA --probe 0d28:0204 --speed 50 \
  --non-interactive --verify --reset target/thumbv7em-none-eabihf/release/nrf-demo-app
```

按本机探针权限使用 sudo 或 udev 规则。原厂 SoftDevice/UF2 与项目 bootloader/app 不混用；旧 dongle 保留作回退。首刷验证项目 VER 后，后续升级使用本文件前述 CDC OTA。

slot 切换现在先保存离开槽的 RAM counter；切到当前 slot 时不从 flash 重载旧 counter。仍须做不足 128 帧快速切回的实际 CNT/ACK 检查及断电测试；构建通过不等于存储掉电验收通过。

## 9. 双端 UART 诊断

`uart-smoke` 是显式诊断 feature，默认仍构建正常 Unifying 应用。使用同一接线、UARTE0、1 Mbps/8N1 和 RX 上拉；不恢复/保存配对 profile、不执行 RF tick、all-up 或无线命令。CDC 和 OTA 保留。

```sh
cargo build --locked -p nrf-demo-app --release --features uart-smoke \
  --target-dir target/uart-smoke
arm-none-eabi-objcopy -O binary \
  target/uart-smoke/thumbv7em-none-eabihf/release/nrf-demo-app \
  target/uart-smoke/thumbv7em-none-eabihf/release/nrf-demo-app.bin
sudo python3 tools/cdc_dfu.py \
  --port /dev/serial/by-id/usb-nrf-demo_nRF52840_Unifying_CDC_0001-if00 \
  --image target/uart-smoke/thumbv7em-none-eabihf/release/nrf-demo-app.bin
```

bootloader 不变，诊断产物不覆盖默认 app。旧版可取消 reset 的固件升级时，需保持 CDC DTR 至 reset 执行；上传工具返回 `REBOOT` 后立即关闭端口可能只留下待换镜像。修正版不再依赖会话保持打开。

开机后每秒发送 `#NRF52840 PING XXXXXXXX<LF>`，从序号 1 开始；接收 Pico 的 `#RP2040 PING XXXXXXXX<LF>`。两端按完整前缀、八位大写十六进制和 LF/CRLF 校验，支持拆包、粘包和损坏后重新同步。发送、接收均不依赖 CDC DTR；序号溢出按 u32 回绕。

```sh
sudo python3 tools/unifying.py --port /dev/serial/by-id/usb-nrf-demo_nRF52840_Unifying_CDC_0001-if00 raw VER
# VER unifying/1 build=<timestamp> uart-smoke
sudo python3 tools/unifying.py --port /dev/serial/by-id/usb-nrf-demo_nRF52840_Unifying_CDC_0001-if00 raw UARTTEST
# UARTTEST rxbytes=<n> peerframes=<n> badframes=<n> uart_errors=<n> lastseq=<XXXXXXXX> txframes=<n>
```

`rxbytes` 是接收字节数，`peerframes` 是完整合法 peer 消息数，`badframes` 是损坏/非法行数，`uart_errors` 是驱动错误次数，`lastseq` 是最近收到的序号，`txframes` 是成功完成 DMA write 的消息数。计数饱和；最近序号保留完整 u32。诊断其余无线命令返回 `ERR DIAGNOSTIC`，OTA 命令仍按原协议处理。

仅 TX 计数增加不是对端收到的证明。两端 peer 计数与最近序号持续变化，才能证明双向收发；零 peer 时同时记录 RX 字节、坏行和硬件错误，不能只归因于接线。

实测：nRF CDC 返回 `peerframes=307 badframes=0 uart_errors=0 lastseq=00000245 txframes=307`；Pico 的 peer 计数约 3 秒从 237 增至 240，最近 nRF 序号从 238 增至 241，UART 错误 0。诊断模式两端运行且双向接收已证明，5 项 nRF 诊断 parser 宿主测试通过；这些证据不证明无线配对、键盘转发或端到端延迟。

重启取消修复版诊断 app 为 28688 字节，经 CDC 写入并实际启动。新版本 CDC 返回 `peerframes=429 badframes=0 uart_errors=0`、`STATE BOOT`。回归实测收到 `REBOOT` 后 4.485ms 即关闭 CDC，早于 100ms reset 延迟；Pico 随后读到 nRF 序号从 483 重新变为 3，完整 peer 计数从 875 增至 879，证明关闭 DTR 不再取消已接受的 reset。实测结束两端仍为诊断模式，不是正常 KVM。

## 10. 正常 KVM 联调记录

两端已从诊断模式恢复默认 KVM 固件。修复前正常启动复现 `FAULT: UART timeout`；Pico 不再 reset 时，通过 SWD 向 UART1 TX FIFO 写入真实编号请求，Pico RX ring 回读到以下 nRF 真实回复：

```text
@00000002 VER unifying/1 build=1790947478
@00000003 STATUS SLOT=0 PAIRED=0 CONN=0 CH=5 CNT=0
@00000004 SWITCHED 0 EMPTY
@00000005 ERR NOTCONN
```

这些请求经过物理 UART、正常 parser、唯一无线 worker 和回复队列，不是模拟 ACK；也不是 Pico KVM 启动状态机自然完成的证明。slot 0 尚未配对，不能报告 READY 或无线成功。

正常 KVM 修复并实机验证：

- RX 上拉从诊断专有配置移入正常 nRF，防止 Pico reset 时 RX 浮空。
- Pico `bridge_init` 先设置 RX idle 和引脚功能，再启用 UART；修复前 RX ring 的首字节 `FE` 污染整条真实回复，修复后第一字节直接是 `@`。没有加入特定字节过滤或盲目重试。
- nRF 为 UART RX 单独通知 CDC epoch 变化，取消旧 DMA 等待；避免等待无数据期间 USB 枚举改变 epoch，导致第一条新请求错误地携带旧 session 并收到 `ERR BUSY`。取消完成后保留已收到 LF 的同步边界，旧部分命令必须丢到 LF，不能由新会话补完。
- 会话取消修复版本为 43440 字节，经 CDC 写入并启动。连续 8 次 Pico reset，其中 3 次在该 app OTA/枚举后，由正常 KVM 状态机自然收到 `@00000001 VER unifying/1 build=1790947478` 和 `@00000002 SWITCHED 0 EMPTY`；没有旧 UART timeout/首帧 ERR BUSY。
- 上述未配对验证时 Pico 因 EMPTY 正确处于 Response fault（debug fault=5/detail=4/state=14），不能强行变 READY。Pico 实机 HID 挂载与协议数组均为 0，真实键盘 mount 和目的端输入尚未验证。

10 项纯 Rust parser 测试通过，正常与诊断 feature 均构建通过。后续端到端联调需要 nRF CDC 接入虚拟机、实际键盘 Host 接入，以及目标 PC/Mac 接收器的配对窗口；保持原 SWD/UART 接线。

### 真实配对与维护恢复

- slot 0 真实配对成功（实测 `PAIRED E7D09A1D26 CH=44`、修复后重新配对 `PAIRED E7D09A1D27 CH=44`）；正常 KVM 自动收到 `SWITCHED 0 CONNECTED` 和 all-up 的编号 `OK`，无 fault，进入 WaitRelease（state=12），因没有 HID mount 而不进入 READY。
- CDC 发送一次 `UKEY 00 04`（按下/释放）返回 `OK`，随后状态 `PAIRED=1 CONN=1 CH=74 CNT=5`。这是 RF/固件 ACK 证据，不代替目的电脑的可见输入记录。
- 实际保持 CDC DTR 时，KVM 收到连续编号 `ERR BUSY` 并进入 Maintenance（state=15）；关闭 DTR 后自然完成 VER → USTATUS → all-up OK → USWITCH CONNECTED → all-up OK，回到无 fault 的 WaitRelease。未模拟无线 ACK。

### 切槽与重启持久化

修复前实测：配对后切到空 slot 1 再回 slot 0，状态变为 `PAIRED=0 CONN=0 CNT=9`，配对仅在 RAM 有效。

原日志把无效记录当空位写，未检查 NOR Flash 的字节是否全为 `0xff`。现只向擦除空位追加；遇到脏无效尾或满页，擦除该 profile 页并保存调用者的当前 profile。活动槽日志同样修正，不改变 32 字节配对格式、不清其他槽。

另外 `build.rs` 的 `rerun-if-changed=memory.x` 使源码改动不更新 `BUILD_TIME`，`VER` 无法区分新旧镜像。该限制已移除；之后用变化的时间戳确认实际运行的构建，旧的“已刷新版本”判断不可靠。

修复后实测（运行 `build=1790958045`）：

- 空槽往返：`USWITCH 01` → `SWITCHED 1 EMPTY`，`USWITCH 00` → `SWITCHED 0 CONNECTED`，随后 `PAIRED=1 CONN=1`，counter 未倒退，`UKEY 00 04` 返回 `OK`。
- nRF 重启后 `STATUS SLOT=0 PAIRED=1`，配对跨重启保留。
- Pico 重启后由 KVM 状态机自然收到 `VER`、`SWITCHED 0 CONNECTED`、all-up `OK`，无 fault，进入 WaitRelease。

掉电（非正常重启）保留仍未测试。

### 维护后 UART 同步屏障

正常 UART 要求启动及每个 CDC 维护窗口后先完成 VER 同步：即使整个 CDC 窗口落在两条 UART 请求之间，也不接受未同步的键击，避免它落入维护期间改动过的 slot。同步前的非 VER 请求回复 `ERR BUSY`，合法 VER 解除屏障，沿用既有 RP 恢复流程。

实测：打开再关闭 CDC（期间 `USTATUS` 正常）后，经 SWD 发送真实 `UKEYDOWN 00 04` 收到 `@00000010 ERR BUSY`；接着 `@00000011 VER` 正常回复，`@00000012 UKEYDOWN 00` 返回 `OK`。无键盘时 Pico 不发请求，该屏障必须在 nRF 侧直接验证。
