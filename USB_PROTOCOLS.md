# USB testers – protocols

This document covers the USB testers that Křižík supports. It describes only what Křižík
implements, not the full analyses behind it.

**None of these devices has been tested with Křižík on real hardware.** The protocols were
read from the vendors' software (and, for FNIRSI, published reverse engineering, checked against
the FNB-C2 firmware's USB descriptors). If one of them misbehaves, a log helps:
`RUST_LOG=krizik=trace krizik`.

| Device | VID:PID | Transport | Implementation |
|---|---|---|---|
| ALIENTEK C0, C2 | `2E88:C2C2` | bulk, interface 0, EP `0x01` OUT / `0x82` IN | `src/atkc.rs` |
| ALIENTEK UT70 | `19F5:6666` | HID, 64-byte reports | `src/ut70.rs` |
| FNIRSI FNB58, FNB-C2 | `2E3C:5558` | HID interface 2, 64-byte reports | `src/fnirsi.rs` |
| FNIRSI FNB48S | `2E3C:0049` | HID | `src/fnirsi.rs` |
| FNIRSI FNB48, C1 | `0483:003A`, `0483:003B` | HID | `src/fnirsi.rs` |

USB access needs the udev rule in `data/udev/70-krizik.rules`. Křižík claims the interface
itself, detaching the kernel's HID driver where there is one.

## ALIENTEK C0 / C2 (ATK-C protocol)

This protocol comes from ALIENTEK's ATK-C desktop software, versions 1.0.1.6 and 1.0.1.7. The USB
code is the same for both testers: the C0 enumerates like a C2, and the two are told apart only by
the version reply.

**Frames.** Host frames are `EE cmd len data… chk FF`, zero padded to 63 bytes, with at least
10 ms between writes. Device frames are `EE cmd len data… chk`. In both directions `chk` makes the
byte sum of the frame 0. Bulk reads can split or merge frames, so the parser carries partial data
over to the next read and skips anything that doesn't check out. Commands are 1–46.

**Session.**
1. Wait 200 ms.
2. Send `01 [00 00]` (version), up to six times, 300 ms apart.
3. Send `03 [00 00 00 00]` (sample-rate query).
4. Send `04 [01 00]` (start streaming).
5. On disconnect, send `04 [00 00]`.

**Version reply.** It has 30 bytes; older firmware sends 29, where every field from offset 4 on
sits one byte earlier.

| Offset | Content |
|---|---|
| 0 | Bootloader version ×10 |
| 1–2 | Firmware: `d1·100 + d2` gives 2.4.4 |
| 3 | Hardware version ×10 |
| 5–6 | Model code (u16 LE; `0x030A` is the C2 family) |
| 7–14 | Serial number |
| 15–26 | Model name |
| 27 | Update mode |

**Sample rate.** Byte 1 of the reply indexes 2, 4, 20, 50, 200, 500 and 1000 samples/s. The rate is
chosen on the tester.

**Live frame.** Command `04` with 57 bytes carries four samples:

| Bytes | Content |
|---|---|
| 0–11 | VBUS, 4 × 24-bit LE, unit 10 µV |
| 12–23 | Current, 4 × 24-bit LE, unit 10 µA |
| 24–31 | D+, 4 × u16 LE, mV |
| 32–39 | D−, 4 × u16 LE, mV |
| 40–47 | CC1, 4 × u16 LE, mV |
| 48–55 | CC2, 4 × u16 LE, mV |
| 56 | Flag, meaning unknown |

Below 500 samples/s the vendor software uses only the first sample of each frame, and so does
Křižík.

**PD monitor.** The tester sends command `19` (55 bytes, ASCII) by itself. It holds the protocol
(0–9), power (10–17), source (18–25), position (26–33), voltage (34–43) and current (44–53).
Křižík shows these on the Live page.

## ALIENTEK UT70

This protocol comes from ALIENTEK's `ATK_UT70DLL` (.NET).

**Reports.** HID reports are 64 bytes and have no report ID.

| Direction | Layout |
|---|---|
| Host to tester | `EE fid len payload… sum FF`, zero padded |
| Tester to host | `EE fid len payload… sum` |

`sum` is the 8-bit sum of all bytes before it.

**Session.** The tester streams by itself once opened. Křižík only sends two requests, both for
the Devices page:

- `02 [00 00]` asks for the version. In the reply, byte 2 is the hardware version ×10 and bytes
  4–11 are the serial number.
- `70 [00 00 00 00]` asks for the sample rate. Byte 1 of the reply indexes 4, 10, 50, 200, 500 and
  1000 samples/s, which is one sample every 250, 100, 20, 5, 2 and 1 ms.

**Live data.** Function `08` carries a 60-byte payload with five sample slots:

| Bytes | Content |
|---|---|
| 0–19 | Voltage, float32 LE |
| 20–39 | Current, float32 LE |
| 40–49 | D+, u16 LE, ÷10000 V |
| 50–59 | D−, u16 LE, ÷10000 V |

Below 500 samples/s only the first slot is used.

## FNIRSI FNB58 family

The wire format follows published reverse engineering (OpenFNB58; baryluk's
fnirsi-usb-power-data-logger). The FNB-C2's firmware (V1.2.11) has the same USB identity, `2E3C:5558`
and "FNB58". It has a CDC interface (0/1) and a HID interface (2) with 64-byte interrupt reports and
no report ID.

**Commands.** A command is `AA cmd 00… crc` (64 bytes). `crc` is a CRC-8 with polynomial 0x39,
initial value 0x42 and no reflection, over bytes 1–62.

| Command | Bytes |
|---|---|
| INIT1 | `AA 81 … 8E` |
| INIT2 | `AA 82 … 96` |
| POLL | `AA 83 … 9E` |

**Session.** The FNB58, FNB48S and FNB-C2 get INIT1, INIT2, INIT2, then POLL every second. The
FNB48 and C1 get INIT1, INIT2, POLL, and are then polled continuously.

**Data.** Data reports start `AA 04` and carry four 15-byte samples at offsets 2, 17, 32 and 47:

| Offset in sample | Content |
|---|---|
| +0 | Voltage, u32 LE, ÷100000 V |
| +4 | Current, u32 LE, ÷100000 A |
| +8 | D+, u16 LE, ÷1000 V |
| +10 | D−, u16 LE, ÷1000 V |
| +12 | Unknown |
| +13 | Temperature, u16 LE, ÷10 °C |

Byte 63 is the same CRC-8. Křižík logs a mismatch but keeps the data. One report arrives every
40 ms, which is 100 samples/s.

## Not supported

| Device | Why |
|---|---|
| ALIENTEK OA1 (cable resistance tester, HID `36B7:FFFF`) | Its PC software only reads device information, calibrates and updates firmware. No measurement results were found in its protocol (frames `DF type len data chk FF`, `chk` = −sum) |
| FNIRSI FNB38, FNAC28 | Encrypted firmware; no published protocol |
