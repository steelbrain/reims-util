# reims-util

[![License: GPL-2.0-or-later OR LGPL-3.0-or-later](https://img.shields.io/badge/License-GPL--2.0--or--later%20OR%20LGPL--3.0--or--later-blue.svg)](LICENSE)

reims-util gets an x86-64 macOS VM ready to boot. It makes:

- **An OpenCore boot image**, with the OVMF firmware to boot it, set up for a q35 VM on KVM.
- **macOS installers** that `reims-ctl` takes as they are, from Apple and checked against Apple's
  own checksums and signatures:
  - an **online installer**, which installs the newest release of macOS 10.13 to 26 over the
    network;
  - an **offline installer**, which installs one exact release of macOS 11 to 26 with no network
    at all.

Downloads pick up where they stopped when they're interrupted.

It's made for [reims-vgpu](https://github.com/steelbrain/reims-vgpu)'s `reims-ctl`, and does the
job you'd otherwise use OSX-KVM or ULTMOS for. It's a single program with no other tools to
install, and it runs on Linux and macOS.

## Install

You need Rust 1.98.1 or later ([rustup.rs](https://rustup.rs)). Then:

```sh
cargo install --locked --git https://github.com/steelbrain/reims-util
```

Or, from a checkout, `cargo install --locked --path .`.

## Make a VM with reims-ctl

```sh
mkdir -p ~/vms/mac && cd ~/vms/mac
reims-util opencore                     # OpenCore.qcow2, OVMF_CODE.fd and OVMF_VARS.fd
reims-util online-installer tahoe       # macOS-Tahoe-online-installer.dmg
reims-ctl create mac --opencore OpenCore.qcow2 \
  --installer macOS-Tahoe-online-installer.dmg --start
```

`reims-ctl` copies the OpenCore image into the VM, finds OVMF beside it, and takes the installer
as it is. OpenCore boots "macOS Base System" with no picker, and you're in macOS
Recovery: erase the VM's disk in Disk Utility, then pick Reinstall macOS, which downloads the rest
from Apple.

## Use it interactively

Run `reims-util` with no arguments. A small interface opens at the bottom of your terminal (it
doesn't take over the screen): pick a job, fill in its form, and press Enter to start it. Esc
stops a download (press it again to leave without waiting), and starting it again picks up where
it left off. What each job wrote, or why it failed, stays in your terminal once you quit.

## Use it from the command line

Everything the interface does is a command too, which is what scripts and `reims-ctl` use.

### OpenCore and OVMF

```sh
reims-util opencore -o OpenCore.qcow2
```

This writes a 128 MiB disk image with OpenCore 1.0.8 on its EFI partition: qcow2 for a `.qcow2`
name, and a raw disk image for anything else (`.img`). Beside it go `OVMF_CODE.fd` and
`OVMF_VARS.fd`, with OVMF's licenses. What goes in them is downloaded from pinned sources
(OpenCore's release and a commit of its picker icons and boot chime, OcBinaryData, from GitHub;
OVMF from Arch Linux's `edk2-ovmf` package), checked against the hashes reims-util knows, and
cached in `~/.cache/reims-util`, so it's only downloaded once.

Each image gets a Mac identity of its own: a serial number, logic board serial (MLB), system UUID
and ROM, which it prints when it's done. Keep them if you'll want the same identity in another
image (`--serial`, `--mlb`, `--uuid`, `--rom`). The model is an iMac20,1 unless you ask for
another with `--model`:

| Model      | Runs                 |
| ---------- | -------------------- |
| iMac20,1   | macOS 10.15.6 to 26  |
| iMacPro1,1 | macOS 10.13 to 15    |

What's in the image, and why:

- **Only the kexts you add** with `--add-kext` ([below](#adding-kexts)), such as reims-vgpu's
  guest kexts. There are no others: QEMU's `isa-applesmc` is the SMC.
- **The drivers a VM boots with**: OpenRuntime, OpenHfsPlus (recovery images and installers are
  HFS+), OpenPartitionDxe (for older recovery images) and ResetNvramEntry.
- **No boot picker**: OpenCore boots the default entry, the disk macOS last chose as its startup
  disk. `--show-picker` shows it at every boot: the graphical one, OpenCanopy, with the GoldenGate
  icons. Pick with the arrow keys, Tab and Enter; it has no mouse pointer in a VM.
  `--text-picker` gives OpenCore's text list instead.
- **The boot chime**, which plays on the VM's Intel HD Audio controller when it has one (QEMU's
  `ich9-intel-hda` with an `hda-output` codec). Turning off "Play sound on startup" in macOS's
  Sound settings silences it.
- **One ACPI table**, with the `EC` and `USBX` devices macOS looks for.
- **The CPU reported as a Skylake-SP**, which every macOS from 10.13 to 26 knows, whatever CPU
  model the VM has. `--no-cpuid-spoof` turns that off.
- **macOS doesn't see it runs in a VM**, so iCloud and the App Store let you sign in.
  `--show-vm` turns that off.
- **System Integrity Protection on**, as on a real Mac. `--disable-sip` turns it off.

`--timeout` sets how long a shown picker waits (0 waits for you), `--boot-args` the kernel's boot
arguments, and `--verbose-boot` shows the kernel's messages while it boots.
`reims-util opencore --help` lists the rest.

#### Adding kexts

`--add-kext` puts a kext bundle of your own in the image. Repeat it for more: they load in the
order you give them, so give a kext after the ones it links against.

```sh
reims-util opencore -o OpenCore.qcow2 \
  --add-kext ~/kexts/Loader.kext,25.4.0,25.99.99 --add-kext ~/kexts/Plugin.kext,25.4.0
```

Each value is `PATH[,MIN[,MAX]]`: the bundle, then the Darwin versions it loads on, as OpenCore's
MinKernel and MaxKernel take them (25.4.0 is macOS 26.4). Left out, they're 25.0.0 and 25.99.99,
macOS 26. The bundle is copied whole into `EFI/OC/Kexts`, without its dotfiles, and OpenCore loads
the executable its Info.plist names, or none for a kext with no code. reims-util refuses a kext
with the name or bundle identifier of one given before it (the image's FAT32 partition doesn't
tell names apart by case), and one that links against a kext given after it.

### Online installers

```sh
reims-util online-installer list
reims-util online-installer sonoma -o ~/vms/mac
```

An online installer is Apple's recovery image for a macOS, as a Mac's Internet Recovery gets it:
about 1 GB, written as `macOS-<name>-online-installer.dmg`. A VM boots it into macOS Recovery,
whose Reinstall macOS downloads and installs the newest release of that macOS. Name a version by
name (`sonoma`, `big-sur`), number (`14`, `10.15`) or `latest`. The image is checked against its
chunklist, which Apple signs, 10 MiB at a time, so what you get is byte for byte what Apple made.

### Offline installers

```sh
reims-util offline-installer list
reims-util offline-installer 26.6.2 -o ~/vms/mac
```

Recovery always installs the newest release of its macOS, downloaded as it goes. To install a
particular one, such as 26.6.2 when 26.7.1 is out, or to install with no network, make an offline
installer: a disk image, `macOS-<name>-<version>-<build>-offline-installer.img`, of 13 to 20 GB.
It holds the recovery image of that macOS, which OpenCore boots as it would from a USB stick, and
Apple's full installer for that release, with a script that installs it, on an HFS+ volume named
Installer: Recovery mounts HFS+ by itself on every macOS from 11 to 26, while the Recovery of
macOS 14 and 15 mounts no exFAT or FAT32 volume by itself.

`offline-installer list` shows the releases Apple's software update catalog has for Intel Macs;
macOS 27 runs on Apple silicon only, so it isn't there. Name one by version (`26.6.2`, or `26` for
the newest 26), build (`25G83`), name (`tahoe`, for the newest Tahoe), Apple's product ID
(`140-93587`) or `latest`. Both of its files are downloaded straight into their places in the
image, each checked against Apple's chunklist for it, so the installer, up to 18 GB, is never on
your disk twice.

```sh
reims-ctl create mac --opencore OpenCore.qcow2 \
  --installer macOS-Tahoe-26.6.2-25G83-offline-installer.img --start
```

reims-ctl uses the image where it is, read-only, so it isn't copied again. Once you're in Recovery:

1. Open Disk Utility and erase the VM's disk as APFS. You can leave its name as it is.
2. Quit Disk Utility, open Utilities > Terminal, and run:

   ```sh
   sh /Volumes/Installer/install-macos.sh
   ```

The script puts "Install macOS ….app" on the disk you erased and starts it; the VM restarts
into the installer, and goes on from there as any macOS install does.

### Interrupted downloads

An installer is written as a `.part` file next to where it ends up. If it stops, whether you
pressed Ctrl-C, the network dropped or the machine went to sleep, run the same command again:
what's there is checked against Apple's chunklists and the download continues from there. An
installer that's already complete and checks out isn't downloaded again, and one that doesn't is
repaired. An offline installer whose recovery image Apple has since replaced is left as it is: it
still boots, and a VM may be using it. One an earlier reims-util made, with its installer on an
exFAT volume, is made again with the installer it already holds, which is checked as a download
is, rather than downloaded again; the old image is replaced once the new one checks out.

### For scripts

`--json` prints progress and the result as JSON lines on stdout. The last line is either
`{"event":"done","result":{…}}`, with what was written (and for `opencore`, the identity it
made, and in `kexts` the bundles it carries, in the order they load) or, for `list`, the
versions, or `{"event":"error","message":"…"}`.

```sh
reims-util --json online-installer tahoe -o ~/vms/mac
```

## License

reims-util is licensed under GPL-2.0-or-later OR LGPL-3.0-or-later; see [LICENSE](LICENSE). The
serial number tables come from OpenCorePkg's macserial, under its BSD-3-Clause license.
OpenCore, its picker icons and chime (OcBinaryData), and OVMF, which reims-util downloads, keep
their own licenses.
