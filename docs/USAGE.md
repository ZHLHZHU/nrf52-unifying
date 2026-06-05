# 使用文档 —— nRF52840 优联键盘发射器

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
