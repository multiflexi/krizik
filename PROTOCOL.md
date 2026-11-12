# ALIENTEK C2 USB tester – Bluetooth LE protocol

Reverse-engineered from the C2 application firmware **V2.5.0** (`C2_APP_V2.5.0.atk`)
and verified against a **C2 Pro** (hardware 1.5) running firmware **V2.4.4**; the
Bluetooth protocol is the same in both versions. The USB protocol used by the
ATK-C desktop software is summarised at the end for reference.

## Transport

The tester contains a transparent BLE-to-UART module (switched and renamed by the
MCU with `AT+SBSW0/1`, `AT+SBN<name>`, `AT+RESET`). It advertises:

* the name stored in the tester (default `C2_BLE…`; the advertised name can be shorter than the stored one),
* service UUID `0000fff0-0000-1000-8000-00805f9b34fb`,
* manufacturer data whose 16-bit key is the first two bytes of its MAC address
  (little endian) and whose value is the remaining four bytes, e.g. MAC
  `12:34:56:78:9A:BC` → key `0x3412`, value `56 78 9A BC`.

GATT service `0xFFF0`:

| Characteristic | Properties                     | Use                           |
|----------------|--------------------------------|-------------------------------|
| `0xFFF1`       | write without response, notify | requests (alternative)        |
| `0xFFF2`       | read, notify                   | **responses** (notifications) |
| `0xFFF3`       | write without response         | **requests**                  |

No pairing or bonding is needed. With a large MTU (BlueZ negotiates 512) a
response arrives in a single notification; with the default MTU the module splits
it, so the client must reassemble frames.

## Frames

```
request : AF  0A 03  cmd  len  data[len]  chk
response: DF  0A 03  cmd  len  data[len]  chk
```

* The identify request and its response use `FF FF` instead of `0A 03`.
* `chk` = two's complement of the 8-bit sum of all preceding bytes, i.e.
  `sum(frame) & 0xFF == 0`.
* The firmware only checks the header byte, `len` and `chk`; bytes 1–2 are ignored.
* The firmware keeps **one** request: a new request replaces a pending one.

## Commands

| cmd    | Request data      | Response data | Meaning |
|--------|-------------------|---------------|---------|
| `0x00` | –                 | `0A 03 <model>` | Identify. Model `'A'` = Pro, `'B'` = Std, `'C'` = Lite. Also enables BLE sample buffering. |
| `0x01` | –                 | `00`          | Enable BLE sample buffering. |
| `0x02` | –                 | 29 bytes, see below | Device information. |
| `0x04` | –                 | 199 bytes, see below | Poll realtime data. Answered when the next block of 10 samples is ready, so a client can poll again as soon as each answer arrives. A 15 s watchdog is re-armed by every poll. |
| `0x0B` | name, ≤ 12 bytes  | `00`          | Store a new Bluetooth name, then send `AT+SBN<name>` and restart the BLE module (the connection drops ~0.6 s later). The firmware formats the AT command with `%s` directly from its receive buffer, so send the name NUL-terminated. |
| `0x0C` | –                 | 12 bytes      | Stored Bluetooth name, NUL padded. |
| other  |                   |               | Treated like `0x04`. |

These six commands are the complete BLE command set of firmware V2.5.0. Protocol
triggering, upload rate, language, screen and offline records are only reachable
over USB (see below).

Captured exchange (C2 Pro, nothing plugged in):

```
→ af ff ff 00 00 53
← df ff ff 00 03 0a 03 41 d2                       model 'A' = C2 Pro
→ af 0a 03 02 00 42
← df 0a 03 02 1d 0a 18 0f 19 0a 03 39 33 30 38 46 46 37 46
     41 54 4b 2d 43 32 20 50 72 6f 00 00 00 00 00 ee
→ af 0a 03 0c 00 38
← df 0a 03 0c 0c 43 32 5f 42 4c 45 30 33 00 00 00 00 f2   "C2_BLE03"
→ af 0a 03 01 00 43
← df 0a 03 01 01 00 12
→ af 0a 03 04 00 40
← df 0a 03 04 c7 …199 bytes… 33
```

### Device information (`0x02`, 29 bytes)

| Offset | Size | Meaning                                       | Example     |
|--------|------|-----------------------------------------------|-------------|
| 0      | 1    | bootloader version ×10                        | `0A` → 1.0  |
| 1      | 1    | firmware series ×10 (major.minor only)        | `18` → 2.4.x|
| 2      | 1    | hardware version ×10                          | `0F` → 1.5  |
| 3      | 1    | byte 1 + 1 (computed by the firmware)         | `19`        |
| 4      | 2    | model code, little endian                     | `0x030A`    |
| 6      | 8    | serial number, ASCII                          | `9308FF7F`  |
| 14     | 12   | model name, ASCII, NUL padded                 | `ATK-C2 Pro`|
| 26     | 1    | bootloader flag                               | 0           |
| 27     | 1    | 1 on the C2 Lite                              | 0           |
| 28     | 1    | reserved                                      | 0           |

Bytes 0–5 come from an info block kept by the bootloader. During an update the
PC software sends the bootloader the 13-byte `.atk` header (bootloader command
`0x11`), whose byte 3 is the release series: every 2.4.x release has `0x18`
(V2.3.9 → `0x17`, V2.4.0–V2.4.3 → `0x18`, V2.5.0 → `0x19`). The firmware sends
that byte as byte 1 and adds one for byte 3, so byte 3 is **not** the firmware
version. The full version (e.g. `2.4.4`) is a string compiled into the firmware
and is only shown on the tester's About screen (and returned over USB); it is
not available over Bluetooth.

### Realtime block (`0x04`, 199 bytes)

| Offset | Size     | Meaning |
|--------|----------|---------|
| 0      | 10       | detected protocol, ASCII (`Protocols` when none) |
| 10     | 8        | advertised total power, ASCII |
| 18     | 8        | gear index / gear count, ASCII (`---NA---` when none) |
| 26     | 8        | gear detail, ASCII |
| 34     | 10       | requested gear (type, voltage), ASCII (`---NA---` when none) |
| 44     | 11       | requested gear detail, ASCII |
| 55     | 1        | power bank mode (periodic dummy load) enabled |
| 56     | 1        | sample interval in ms (20 → 50 samples/s) |
| 57     | 10 × 3   | VBUS, 24-bit little endian, unit 10 µV |
| 87     | 10 × 3   | current, 24-bit little endian, unit 10 µA |
| 117    | 10 × 2   | D+, mV |
| 137    | 10 × 2   | D−, mV |
| 157    | 10 × 2   | CC1, mV |
| 177    | 10 × 2   | CC2, mV |
| 197    | 1        | status nibble (meaning unknown) |
| 198    | 1        | tester temperature, °C |

The sample interval is selected on the tester itself (▶ button on the basic
measurement screen); at the default 20 ms a block arrives every 200 ms.

## Firmware file format (`.atk`)

A list of sections, each a 13-byte header followed by the data, padded to a
multiple of 4 bytes. Header: device type `0A 03` (0x030A), minimum hardware
version ×10 (`0A`), release series ×10 (`19` = 2.5.x), `A0`, chunk size for the
bootloader (`38` = 56), data length (32-bit LE) and the build date as `YY MM DD`
(`1A 08 1C` = 2026-08-28). It is followed by the application image
for an HDSC HC32 Cortex-M MCU linked at `0x00008000`, XOR-obfuscated with the
repeating 4-byte key `43 DF 2A 9B`.

## Firmware update over USB

Bluetooth has no firmware-update command; updates go through the PC port.
Both USB modes use interface 0 with bulk endpoints `0x01` (OUT) and `0x82` (IN).

1. In application mode (`2E88:C2C2`) send `EE 0F 02 01 00 00 FF` (zero padded to
   63 bytes). The tester restarts as `2E3C:5780`.
2. Bootloader frames: `AF cmd seq len data… chk`, `seq` counting up from 0 per
   frame. Replies: `DF cmd …` with a status byte at offset 4 (0 = OK).

| cmd    | data                        | reply / meaning |
|--------|-----------------------------|-----------------|
| `0x10` | –                           | device type (u16 LE at offset 4) and hardware version (offset 6) |
| `0x11` | 13-byte section header      | status |
| `0x12` | –                           | status; start of the section |
| `0x13` | `chunk size` bytes of data  | status; data sent as stored in the file (obfuscated) |
| `0x14` | –                           | status; end of the section |
| `0x15` | `A0`                        | status; start the application |

The PC software checks that the device type equals the file's and that the
hardware version is at least the file's minimum, sends `0x11`–`0x14` for every
section of the file, then `0x15`. Every data frame carries a full chunk; the
last one is padded (the 2.5.0 image is an exact multiple of 56 bytes).

The application-mode version reply (command `0x01`, 30 bytes) contains the
exact firmware version: bytes 1 and 2 (`2, 44` → 2.4.4), followed by the
hardware version, model code, serial number and model name as in the Bluetooth
device information.

## USB protocol (ATK-C desktop software), for reference

USB device `2E88:C2C2` (bootloader `2E3C:5780`), 63-byte bulk/interrupt packets:

```
EE cmd len data[len] chk FF   (zero padded to 63 bytes)
```

with the same checksum. Commands (from the ATK-C binary): `01` version, `02` get/set
screen, `03` get/set upload rate (codes 0–6 → 2, 4, 20, 50, 200, 500, 1000 SPS),
`04` start/stop streaming (57-byte packets with 4 samples), `05` trigger
fast-charging protocol, `06` automatic protocol detection, `07` language, `08`–`0A`
offline records, `0B` data recording switch, `0C`/`0D` PD packet capture, `0F` enter
bootloader, `16`–`18` e-marker read/write/delete, `19` PD monitor info, `21`/`22`
C-line info. None of these are accepted over Bluetooth.
