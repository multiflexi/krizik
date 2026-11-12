# OWON Bluetooth multimeters – Bluetooth LE protocol

This was reverse-engineered from OWON's Android app "Multimeter BLE" V1.6.5.1. That
build is sold under several brands; its package names are `com.lilliput.Multimeter`
and `com.owon.MultimeterBLE`, and the OEM build analysed was `com.neutral.MultimeterBLE`.
OWON's newer Flutter app "iMeter" uses the same characteristics and commands.

Křižík implements this protocol in `src/owon.rs` and `src/ble.rs`. It targets
the **B35T+** and **B41T+**. The other families are handled as the vendor app handles
them, but have not been tested.

Points marked *unverified* follow the vendor app's code. They have not yet been
checked against a meter.

## Transport

The meter advertises GATT service `0000fff0-0000-1000-8000-00805f9b34fb`, which is
the same UUID as the ALIENTEK C2 tester. Only the meter has characteristic `0xFFF4`,
so Křižík tells the two apart by that characteristic once connected. The advertised name is usually
`BDM` (*unverified*).

| Characteristic | Use |
|----------------|-----|
| `0xFFF1` | Identity check and 16-byte text commands (write). Some commands are answered by reading `0xFFF1` back. |
| `0xFFF2` | Device information (read) |
| `0xFFF3` | Remote buttons (write) |
| `0xFFF4` | Measurements and the offline log (notify) |
| `0xFFF5` | Unused |

The vendor app never sets a write type. Křižík writes without response when a
characteristic allows it, and with response otherwise. No pairing is needed.

## Device information (`0xFFF2`)

| Offset | Meaning |
|--------|---------|
| 0 | Series: `0x21` B33, `0x23` B35, `0x29` B41, `0x12` OW18, `0x14` OW20, `0x37` OW55 |
| 1 | Battery in percent. The app only uses it on meters with an offline log, and only when it is 0–100. |
| 2–4 | Firmware version as major, minor, patch. The app compares `v = b2·100 + b3·10 + b4`: renaming needs v ≥ 11, and a dated log needs v ≥ 12. |
| 5 | Offline log: `FF` = not supported, `00` = supported, `01` = recording now |

The app reads this characteristic after connecting and then every 6 s; Křižík reads it every 10 s.

A series `0x23` meter without an offline log is the original **B35T**. It sends
14-byte ASCII frames instead of the 6-byte binary frames.

## Identity check

The vendor apps run this check once per connection, after the first information read:

1. Pick six random numbers `c0…c5` in 0–35.
2. Write `c[i] + [200, 100, 50, 20, 10, 5][i]` (8-bit, wrapping) to `0xFFF1`. Pad it
   with zeros to 16 bytes; the original B35T takes the six bytes only.
3. Read `0xFFF1`. The result must be the raw MD5 digest of the six characters
   `s1[c0] s1[c1] s1[c2] s2[c3] s2[c4] s2[c5]`, where:

   ```
   s1 = zyx0wv1uts2rq3pon4ml5kji6hg7fed8cb9a
   s2 = ab9cde8fg7hij6kl5mno4pq3rst2uv1wxy0z
   ```

On a mismatch the app drops all data and disconnects, but the meter is not told about it.
Whether the meter streams without the check is *unverified*. Křižík performs the
check and only logs a mismatch.

| Written | String | Expected reply |
|---------|--------|----------------|
| `c8 64 32 14 0a 05` + 10 × `00` | `zzzaaa` | `4E2F41BD78313C128F1316ED4CB5169D` |
| `c9 66 35 18 0f 0b` | `yx0de8` | `2ACAAD8E09BBEBF71D307BA98932D61D` |

## Measurements (`0xFFF4`)

Notifications are concatenated, and frames are cut from the start of the buffer.
A partial frame is kept for the next notification. There is no header to resynchronise
on, so Křižík drops a leftover fragment when a notification of whole frames arrives.

### 6-byte binary frame (all meters except the original B35T)

The frame is three little-endian 16-bit words.

| Word | Bits | Meaning |
|------|------|---------|
| w0 | 0–2 | decimal point: 0–4 = digits after the point, 5 = error, 6 = UL (underload), 7 = OL (overload) |
| w0 | 3–5 | prefix: p, n, µ, m, (none), k, M, G |
| w0 | 6–9 | function, see below |
| w0 | 10 | OW55 only: sign |
| w1 | 0–13 | flags: HOLD, REL, AUTO, low battery, MIN, MAX, OL, RMR, PMIN, PMAX, UL, LPF0, LPF1, VBAR |
| w2 | 15 | sign (except OW55) |
| w2 | 0–14 | magnitude, the displayed digits (OW55: all of w2, as a signed number) |

Function codes:

| Code | Function | Code | Function |
|------|----------|------|----------|
| 0 | DC V | 7 | duty cycle % (prefix ignored) |
| 1 | AC V | 8 | °C |
| 2 | DC A | 9 | °F |
| 3 | AC A | 10 | diode, V |
| 4 | Ω | 11 | continuity, Ω |
| 5 | F | 12 | transistor hFE |
| 6 | Hz | 13 | NCV on OW18/OW20/OW55 (value 0–4 = "EF", "-" … "----"), "ADP" otherwise |

Example: `1B F0 04 00 D2 84` is w0 = `0xF01B`, which means three decimals, prefix m and
function DC V. w1 is `0x0004`, so AUTO is on. w2 is `0x84D2`: the sign bit is set and
the magnitude is 1234. The reading is **−1.234 mV DC, AUTO**.

### 14-byte ASCII frame (original B35T)

The frame is `±dddd␠P S1 S2 S3 S4 B CR LF`, in the style of the FS9922 meter chip:

- A sign character, then four digits. Non-digits mean overload, e.g. `?0:?`.
- `P` is the decimal point: `1` = d.ddd, `2` = dd.dd, `4` = ddd.d, otherwise none.
- `S1`: `0x20` AUTO, `0x10` DC, `0x08` AC, `0x04` REL, `0x02` HOLD.
- `S2`: `0x20` MAX, `0x10` MIN, `0x04` low battery, `0x02` prefix n.
- `S3`: `0x80` µ, `0x40` m, `0x20` k, `0x10` M, `0x08` continuity, `0x04` diode, `0x02` %.
- `S4`: `0x80` V, `0x40` A, `0x20` Ω, `0x10` hFE, `0x08` Hz, `0x04` F, `0x02` °C, `0x01` °F.
- `B` is the bar graph; it is ignored.

Example: `2D 31 32 33 34 20 31 30 00 40 80 00 0D 0A` is −1.234 mV DC, AUTO.

## Remote buttons (`0xFFF3`)

Each press is written as two bytes: `[button, 1]` for a short press and `[button, 0]` for a long press.

| Code | Button |
|------|--------|
| 1 | Select |
| 2 | Range |
| 3 | Hold |
| 4 | Rel. A long press asks the meter to switch its Bluetooth off; Křižík doesn't offer it. |
| 5 | Hz/Duty |
| 6 | Max/Min (B35, B41; the OW18/OW20/OW55 send 3 instead) |

## Text commands (`0xFFF1`)

All commands are 16 bytes long, zero-padded.

| Command | Meaning |
|---------|---------|
| `@` name | New Bluetooth name, at most 15 bytes. Needs firmware ≥ 1.1 (v ≥ 11). iMeter only allows letters, digits and `_`. |
| `*DATe` + `20 yy MM dd hh mm ss` | Sets the meter's clock, sent before `*RECOrd` when firmware v ≥ 12. The values are binary; the hour is 1–24, so midnight is 24. |
| `*RECOrd,` + interval + count | Records `count` readings (1–10 000), one every `interval` seconds (1–4 200 000). Both values are u32, little endian. |
| `*STOP` | Stops recording. |
| `*READlen?` | Then read `0xFFF1`: bytes 0–3 are the log size in bytes (u32 LE). |
| `*READ1?` / `*READ?` | Sends the log over `0xFFF4`: `*READ1?` when firmware v ≥ 12, `*READ?` otherwise. |

Example: `*DATe` for 2026-11-11 14:05:09 is
`2a 44 41 54 65 14 1a 0b 0b 0e 05 09 00 00 00 00`.

## Offline recording

After `*RECOrd` the vendor app disconnects. Its help text says that the
meter's Bluetooth symbol disappears until the recording is complete. The app
never downloads while byte 5 of the information is `01`; it offers `*STOP` first.

A download goes:

1. Send `*READlen?` and read the size, which is used for progress.
2. Send `*READ1?` or `*READ?`. From now on, `0xFFF4` carries the log instead of measurements.
3. Split the stream into little-endian 16-bit words, carrying an odd byte over to the next notification.
4. The transfer starts after ten `FF FF` words and ends after ten more. Any `FF FF` inside the data counts towards the end.
5. Interpret the captured data:
   - With firmware v ≥ 12 it starts with a 16-byte header:
     - bytes 0–6: the start time `20 yy MM dd hh mm ss`;
     - byte 7: ignored;
     - bytes 8–11: the interval in seconds (u32 LE);
     - bytes 12–15: the length (u32 LE).

     Older firmware only has the 4-byte length.
   - A word whose high byte is `0xF0`–`0xF3` (`hi & 0xFC == 0xF0`) has the layout of w0. It sets the function, prefix and decimal point for the following values.
   - Every other word is a value, decoded like w2.
6. Return to measurements.

The vendor app has no timeout; Křižík gives up after 8 s without data.

Timestamps (*unverified*): the vendor app gives the k-th word of the data the time
`start + k × interval`. That count includes the function words, so a range change
shifts every later reading by one interval. Křižík instead numbers only the
values, as `start + (n + 1) × interval`. The two agree when the log starts with a
single function word.
