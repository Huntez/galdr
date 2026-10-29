# Galdr protocol, version 1

How the Galdr phone app and the Galdr PC client find each other and stream audio over a local
network. All integers are big-endian. Text is UTF-8.

## Overview

| Channel | Transport | Listens | Default port |
|---|---|---|---|
| discovery | UDP | phone | 47800 |
| control | TCP | phone | 47800 |
| media | UDP | PC | 47801 (sent to the phone in `HELLO`) |

1. *(Optional)* The PC broadcasts a discovery request; every phone that is running answers
   with its name and control port.
2. The PC opens a UDP socket on its media port, connects to the phone over TCP and sends
   `HELLO`.
3. The phone answers `WELCOME` with the audio format and starts sending media packets to the
   PC's address (the one the TCP connection came from) on the media port.
4. Both sides send `PING` every second. A side that hears nothing on the control connection for
   5 seconds closes it.
5. Either side ends the session by closing the TCP connection. The phone serves one PC at a
   time: a new connection replaces the old one.

## Discovery (UDP)

Request, PC → broadcast address, discovery port:

| Offset | Size | Field |
|---|---|---|
| 0 | 4 | `GLDR` |
| 4 | 1 | `0x10` discovery request |
| 5 | 1 | protocol version (`1`) |

Reply, phone → the request's source address and port:

| Offset | Size | Field |
|---|---|---|
| 0 | 4 | `GLDR` |
| 4 | 1 | `0x11` discovery reply |
| 5 | 1 | protocol version (`1`) |
| 6 | 2 | control port (u16) |
| 8 | 1 | length *n* of the phone name |
| 9 | *n* | phone name, e.g. `Pixel 7` |
| 9 + *n* | … | device details for display, e.g. `Android 16` (may be empty) |

The phone answers only while it is ready to stream (Start pressed). Requests with another
version or a wrong magic get no reply.

## Control channel (TCP)

Message: `type:u8 | len:u16 | payload[len]`. Unknown types are ignored, so later versions can
add messages.

| type | Name | Direction | Payload |
|---|---|---|---|
| `0x01` | `HELLO` | PC → phone | `GLDR`, version u8, media port u16, PC name |
| `0x02` | `WELCOME` | phone → PC | `GLDR`, version u8, codec u8, sample rate u32, channels u8, frame ms u8, phone name |
| `0x03` | `REJECT` | phone → PC | reason text; the phone then closes the connection |
| `0x04` | `PING` | both | — |

`HELLO` is the first message on a connection. The phone answers `WELCOME`, or `REJECT` when it
does not speak the PC's version. Codecs: `1` = Opus. Version 1 phones send Opus, 48000 Hz,
1 channel, 20 ms frames.

Example (PC "DESKTOP", media port 47801; phone "Pixel 7"):

```
PC → 01 000e 474c4452 01 bab9 4445534b544f50
   ← 02 0013 474c4452 01 01 0000bb80 01 14 506978656c2037
PC → 04 0000                (every second, and the phone likewise)
```

## Media channel (UDP)

One datagram is one encoded frame, phone → PC media port:

| Offset | Size | Field |
|---|---|---|
| 0 | 4 | `GLDR` |
| 4 | 1 | `0x20` media |
| 5 | 1 | flags (none defined yet, send `0`) |
| 6 | 4 | sequence number (u32), +1 per packet, starts at 0 |
| 10 | 4 | timestamp (u32) of the frame's first sample, in samples |
| 14 | … | encoded frame (one Opus packet) |

The receiver uses the sequence number to detect loss and reordering; packets older than the
last one played are dropped.
