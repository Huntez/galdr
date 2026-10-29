<h1 align="center">
  <img src="assets/banner.png" alt="" width="560"><br>
  galdr
</h1>

<p align="center">Use your phone as a PC microphone over Wi-Fi.</p>

<p align="center">
  <img alt="PC: Windows 10/11" src="https://img.shields.io/badge/PC-Windows%2010%2F11-0078D6">
  <img alt="Phone: Android 11+" src="https://img.shields.io/badge/phone-Android%2011%2B-3DDC84?logo=android&logoColor=white">
  <img alt="Connection: Wi-Fi or USB" src="https://img.shields.io/badge/connection-Wi--Fi%20%C2%B7%20USB-555555">
  <img alt="Audio: 48 kHz, Opus up to 256 kbit/s" src="https://img.shields.io/badge/audio-48%20kHz%20%C2%B7%20Opus%20256%20kbit%2Fs-E6A028">
  <br>
  <img alt="Rust" src="https://img.shields.io/badge/Rust-B7410E?logo=rust&logoColor=white">
  <img alt="Kotlin" src="https://img.shields.io/badge/Kotlin-7F52FF?logo=kotlin&logoColor=white">
  <img alt="License: MIT" src="https://img.shields.io/badge/license-MIT-E6A028">
  <br>
  <img alt="Linux: planned" src="https://img.shields.io/badge/Linux-planned-lightgrey?logo=linux&logoColor=white">
  <img alt="Bluetooth: planned" src="https://img.shields.io/badge/Bluetooth-planned-lightgrey">
</p>

Two parts: an Android app that captures the microphone and a PC client that turns it into a
microphone for other programs.

- **Real microphone input** — raw capture, or Android noise suppression with one switch
- **Finds the phone by itself** — no IP to type on the same network
- **Reconnects on its own** — after a Wi-Fi drop or a restart of the phone app
- **Works with any app** — through VB-CABLE or any playback device
- **Listen to myself** — switchable while connected
- **Volume on the phone, level meter on the PC**, tray icon, remembered settings
- **Command line version** for scripts

## Requirements

- **Phone:** Android 11 or newer, on the same Wi-Fi as the PC or on a USB cable.
- **PC:** Windows 10 or 11 (tested on 11) and a virtual microphone, so other programs can use
  the sound (see below). Without one, Galdr can still play to your speakers.

## Setup

### 1. Install a virtual microphone (once)

Windows has no way for a program to create a microphone by itself, so Galdr needs a driver
that provides one. **Recommended: [VB-CABLE](https://vb-audio.com/Cable/)** (free, donationware).

1. Download *VBCABLE_Driver_Pack* from [vb-audio.com/Cable](https://vb-audio.com/Cable/) and unzip it.
2. Right-click `VBCABLE_Setup_x64.exe` → *Run as administrator* → *Install Driver*.
3. Restart the PC.

You now have **CABLE Input** (Galdr plays into it) and **CABLE Output** (the microphone other
programs see). Other virtual cables work too; Galdr lists the ones it finds.

### 2. Connect

1. **Phone:** open Galdr and press **Start**.
2. **PC:** run `galdr-gui.exe`. Your phone shows up under *Phones* within a few seconds (or
   type the IP the phone shows under *Enter IP*). Pick **VB-Audio Virtual Cable** under
   *Microphone* and press **Connect** next to the phone.
3. **In Discord, OBS, Zoom, …:** choose **CABLE Output (VB-Audio Virtual Cable)** as the
   microphone.

Tick *Listen to myself* to hear what is sent.

### USB instead of Wi-Fi

A cable gives a steady, lower delay: Wi-Fi delivers packets unevenly, so Galdr has to buffer
more. Galdr talks to the phone through **adb**, Android's USB debugging tool.

1. **Phone:** turn on *Developer options* (Settings → About phone → tap *Build number* seven
   times), then *Developer options* → **USB debugging**.
2. **PC:** get adb: unzip [Android SDK Platform-Tools](https://developer.android.com/tools/releases/platform-tools)
   and put `adb.exe` (with the two `AdbWin*.dll` next to it) beside `galdr-gui.exe`, or anywhere
   on `PATH`.
3. Plug the phone in and allow USB debugging when it asks. Press **Start** in Galdr on the
   phone; it shows up under *Phones* as *USB*.

### If there is no sound

- **Galdr shows *packets 0* or *Reconnecting: no audio for 5s*, and the phone keeps returning
  to *Waiting for the PC*:** a firewall blocks the audio (UDP port 47801). Allow `galdr-gui.exe` on private
  networks — in Windows Firewall and in tools such as Portmaster, GlassWire or antivirus
  suites.
- **The phone does not show up on the PC:** some routers block the search (guest networks,
  "AP isolation"). Type the IP the phone shows instead.
- **Nothing connects:** check that both devices are on the same Wi-Fi and that the PC uses the
  IP the phone shows. A VPN on the phone is fine as long as it lets local traffic through.

## Sound quality

The phone captures 48 kHz mono and sends Opus (up to 256 kbit/s); the PC decodes it to 32-bit
float. Through VB-CABLE the path is deeper than 16 bits: a −110 dBFS test tone, below 16-bit
resolution, arrives intact. Check with
`cargo run --release --example loopback -- "<cable input>" "<cable output>" 0.000003`.

## Command line

```
galdr find
galdr devices
galdr connect 192.168.1.20 --device "CABLE Input" --monitor
galdr connect usb --device "CABLE Input"
```

`find` lists phones that pressed Start, and those on USB. `usb` connects to the phone on the
cable (`usb:<serial>` picks one of several). `--device <name>` plays into a device (part of its
name is enough), `--monitor` also plays to the speakers. See `galdr connect --help`.

## Plans

- **Linux** PC client (the virtual microphone via PipeWire).
- **Bluetooth** besides Wi-Fi and USB.
- Possibly **an own Windows microphone driver**, so no virtual cable is needed.

## Building

**PC:** Rust (stable, MSVC) and CMake (libopus is built from source):

```
cargo build --release
```

**Android:** JDK 17+, the Android SDK with NDK and CMake (or open `android/` in Android Studio).
libopus comes as a git submodule, so clone with `--recursive` or run
`git submodule update --init` first:

```
cd android
gradlew assembleDebug
```

## Protocol

See [docs/PROTOCOL.md](docs/PROTOCOL.md).

## License

*Not affiliated with VB-Audio.*

MIT. The Android app bundles the Ubuntu font (Ubuntu Font Licence) and Hack (MIT, Bitstream Vera licence).
