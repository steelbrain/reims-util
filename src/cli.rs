//! The command line: each job as a subcommand, for scripts and for reims-ctl. With `--json`,
//! progress and the result are JSON lines on stdout (progress.rs), and the last line is a `done`
//! event with what was written, or an `error` event.

use crate::opencore::{self, added, config, smbios};
use crate::progress::{self, JsonReporter, Reporter, TextReporter};
use crate::{installer, recovery};
use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use serde::Serialize;
use std::path::PathBuf;

const AFTER_HELP: &str = "\
Run it with no command for the interactive interface.

Examples:
  # OpenCore.qcow2, with OVMF_CODE.fd and OVMF_VARS.fd beside it
  reims-util opencore -o ~/vms/mac/OpenCore.qcow2
  # Installers, and what versions each has
  reims-util online-installer tahoe -o ~/vms/mac
  reims-util offline-installer 26.7.1 -o ~/vms/mac
  reims-util offline-installer list

Downloads resume: run the same command again after an interruption.";

#[derive(Parser)]
#[command(
    name = "reims-util",
    version,
    about = "Make OpenCore boot images and OVMF for x86-64 macOS VMs, and macOS installers from \
             Apple's own files: online, or offline for one exact release",
    after_help = AFTER_HELP
)]
pub struct Cli {
    /// Print progress and the result as JSON lines on stdout
    #[arg(long, global = true)]
    pub json: bool,
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand)]
pub enum Command {
    /// Make an OpenCore boot image, and write OVMF beside it
    #[command(after_help = OPENCORE_HELP)]
    Opencore(OpencoreArgs),
    /// Make an online installer: Apple's recovery image, which installs the newest release of
    /// its macOS (10.13 to 26) over the network
    #[command(after_help = ONLINE_HELP)]
    OnlineInstaller(InstallerArgs),
    /// Make an offline installer: a disk image that installs an exact release of macOS (11 to
    /// 26) with no network
    #[command(after_help = OFFLINE_HELP)]
    OfflineInstaller(InstallerArgs),
}

const OPENCORE_HELP: &str = "\
The image is a 128 MiB disk with OpenCore 1.0.8 on its EFI partition, set up for a q35 VM on KVM:
a QEMU disk to boot first, ahead of the macOS disk and the installer. Each image gets an SMBIOS
identity of its own (serial, MLB, UUID, ROM), which it prints; give one to keep it. OVMF_CODE.fd
and OVMF_VARS.fd go beside the image, where reims-ctl finds them.

  reims-util opencore -o OpenCore.qcow2
  reims-ctl create mac --opencore OpenCore.qcow2 --installer macOS-Tahoe-online-installer.dmg";

#[derive(Args, Clone, Debug)]
pub struct OpencoreArgs {
    /// The image to write: qcow2 for a .qcow2 name, else a raw disk image
    #[arg(short, long, default_value = "OpenCore.qcow2")]
    pub output: PathBuf,
    /// The Mac model to claim to be: iMac20,1 (macOS 10.15.6 to 26) or iMacPro1,1 (10.13 to 15)
    #[arg(long, default_value = "iMac20,1")]
    pub model: String,
    /// The serial number (default: a new one for the model)
    #[arg(long)]
    pub serial: Option<String>,
    /// The logic board serial (default: a new one, made for the serial)
    #[arg(long)]
    pub mlb: Option<String>,
    /// The system UUID (default: a new one)
    #[arg(long)]
    pub uuid: Option<String>,
    /// The ROM: 12 hex digits or a MAC address, such as the VM's (default: random)
    #[arg(long)]
    pub rom: Option<String>,
    /// Seconds the boot picker, when shown, waits before it boots the default entry (0 waits for
    /// you)
    #[arg(long, default_value_t = 5)]
    pub timeout: u32,
    /// The kernel's boot arguments
    #[arg(long, default_value = config::DEFAULT_BOOT_ARGS)]
    pub boot_args: String,
    /// Show the kernel's messages while it boots (adds -v to the boot arguments)
    #[arg(long)]
    pub verbose_boot: bool,
    /// Turn System Integrity Protection off
    #[arg(long)]
    pub disable_sip: bool,
    /// Report the VM's own CPUID leaf 1 instead of a Skylake-SP's
    #[arg(long)]
    pub no_cpuid_spoof: bool,
    /// Let macOS see that it runs in a VM (iCloud and the App Store then refuse to sign in)
    #[arg(long)]
    pub show_vm: bool,
    /// Use OpenCore's text picker instead of the graphical one
    #[arg(long)]
    pub text_picker: bool,
    /// Show OpenCore's boot picker at every boot (by default OpenCore boots the default entry)
    #[arg(long)]
    pub show_picker: bool,
    /// Leave out Lilu: the image carries only the kexts given with --add-kext, such as a Lilu of
    /// your own, or none
    #[arg(long)]
    pub no_kexts: bool,
    /// Add a kext bundle, loaded after Lilu on Darwin MIN to MAX (default 25.0.0 to 25.99.99,
    /// macOS 26). Repeat it for more: they load in the order given
    #[arg(long = "add-kext", value_name = "PATH[,MIN[,MAX]]", value_parser = added::parse)]
    pub add_kext: Vec<added::Spec>,
    /// Accepted and ignored: --no-apv-arm64-port and --no-video, whose kexts images no longer
    /// carry
    #[arg(
        long = "no-apv-arm64-port",
        alias = "no-video",
        hide = true,
        action = clap::ArgAction::Count
    )]
    pub _retired: u8,
    /// Write no OVMF files
    #[arg(long)]
    pub no_ovmf: bool,
    /// Where OVMF_CODE.fd and OVMF_VARS.fd go (default: beside the image)
    #[arg(long)]
    pub ovmf_dir: Option<PathBuf>,
}

#[derive(Args, Clone, Debug)]
pub struct InstallerArgs {
    /// The version to make, or `list` (or nothing) to list them
    version: Option<String>,
    /// The folder to write to
    #[arg(short, long, default_value = ".")]
    output: PathBuf,
}

const ONLINE_HELP: &str = "\
VERSION is a name (tahoe, big-sur), a number (26, 10.15) or latest. The installer is
macOS-<name>-online-installer.dmg, as Apple serves it, checked against Apple's signature.

  reims-util online-installer tahoe
  reims-ctl create mac --opencore OpenCore.qcow2 --installer macOS-Tahoe-online-installer.dmg

In Recovery, erase the VM's disk in Disk Utility (APFS), then pick Reinstall macOS.";

const OFFLINE_HELP: &str = "\
VERSION is a version (26.7.1, or 26 for the newest 26), a build (25G241), Apple's product ID,
a name (tahoe, for the newest Tahoe) or latest. The installer is
macOS-<name>-<version>-<build>-offline-installer.img, a disk image of two volumes: the
recovery image of the same macOS, which OpenCore boots, and Apple's full installer, with a
script that installs it.

  reims-util offline-installer 26.7.1
  reims-ctl create mac --opencore OpenCore.qcow2 \\
    --installer macOS-Tahoe-26.7.1-25G241-offline-installer.img

In Recovery, erase the VM's disk in Disk Utility (APFS), then open Utilities > Terminal and
run:

  sh /Volumes/Installer/install-macos.sh";

/// Whether an installer command lists, rather than makes, the versions.
fn lists(version: &Option<String>) -> bool {
    version
        .as_deref()
        .is_none_or(|v| v.trim().eq_ignore_ascii_case("list"))
}

#[derive(Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum Final<T: Serialize> {
    Done { result: T },
    Error { message: String },
}

pub fn run(command: Command, json: bool) -> Result<()> {
    let text = TextReporter::new();
    let js = JsonReporter::new();
    let r: &dyn Reporter = if json { &js } else { &text };
    let result = run_command(command, json, r);
    if let Err(e) = &result
        && json
    {
        progress::json_line(&Final::<()>::Error {
            message: format!("{e:#}"),
        });
    }
    result
}

fn run_command(command: Command, json: bool, r: &dyn Reporter) -> Result<()> {
    match command {
        Command::Opencore(a) => opencore_options(&a).and_then(|o| {
            let out = opencore::build(&o, r)?;
            if !json {
                print_opencore(&out);
            }
            finish(json, &out)
        }),
        Command::OnlineInstaller(a) if lists(&a.version) => {
            #[derive(Serialize)]
            struct V {
                name: String,
                slug: &'static str,
                board_id: &'static str,
            }
            let vs: Vec<V> = recovery::VERSIONS
                .iter()
                .map(|v| V {
                    name: v.title(),
                    slug: v.slug,
                    board_id: v.board_id,
                })
                .collect();
            if !json {
                for v in &vs {
                    println!("{:<14} {}", v.slug, v.name);
                }
            }
            finish(json, &vs)
        }
        Command::OnlineInstaller(a) => {
            let version = a.version.unwrap_or_default();
            let v = recovery::find(&version).with_context(|| {
                format!(
                    "no online installer for {version:?}: `reims-util online-installer list` \
                     lists them"
                )
            })?;
            let out = recovery::online_installer(v, &a.output, r)?;
            if !json {
                println!("{}", out.image.display());
                eprintln!(
                    "\nTo install {} in a VM:\n  reims-ctl create mac --opencore OpenCore.qcow2 \
                     --installer {}\nthen, in Recovery, erase the VM's disk in Disk Utility \
                     (APFS) and pick Reinstall macOS.",
                    out.title,
                    out.image.display()
                );
            }
            finish(json, &out)
        }
        Command::OfflineInstaller(a) if lists(&a.version) => {
            let list = installer::list(r)?;
            if !json {
                for i in &list {
                    println!(
                        "{:<9} {:<8} {:<16} {:>9}  posted {}  ({})",
                        i.version,
                        i.build,
                        i.title,
                        progress::bytes(i.size),
                        i.posted,
                        i.product
                    );
                }
            }
            finish(json, &list)
        }
        Command::OfflineInstaller(a) => {
            let version = a.version.unwrap_or_default();
            let list = installer::list(r)?;
            let i = installer::find(&list, &version).with_context(|| {
                format!(
                    "no offline installer for {version:?}: `reims-util offline-installer list` \
                     lists them"
                )
            })?;
            let out = installer::offline_installer(i, &a.output, r)?;
            if !json {
                println!("{}", out.image.display());
                eprintln!(
                    "\nTo install {} in a VM:\n  reims-ctl create mac --opencore OpenCore.qcow2 \
                     --installer {}\nthen, in Recovery, erase the VM's disk in Disk Utility \
                     (APFS), open Utilities > Terminal and run:\n  {}",
                    out.installer.label(),
                    out.image.display(),
                    installer::IMAGE_COMMAND
                );
            }
            finish(json, &out)
        }
    }
}

fn finish<T: Serialize>(json: bool, v: &T) -> Result<()> {
    if json {
        progress::json_line(&Final::Done { result: v });
    }
    Ok(())
}

pub fn opencore_options(a: &OpencoreArgs) -> Result<opencore::Options> {
    let m = smbios::model(&a.model).with_context(|| {
        format!(
            "unknown model {:?}: {} are known",
            a.model,
            smbios::MODELS
                .iter()
                .map(|m| m.name)
                .collect::<Vec<_>>()
                .join(" and ")
        )
    })?;
    let mut id = smbios::generate(m)?;
    if let Some(s) = &a.serial {
        let s = s.trim().to_ascii_uppercase();
        if s.len() != 12 {
            bail!("a serial number here is 12 characters, not {s:?}");
        }
        id.serial = s;
        if a.mlb.is_none() {
            id.mlb = smbios::mlb_for(m, &id.serial)?;
        }
    }
    if let Some(s) = &a.mlb {
        let s = s.trim().to_ascii_uppercase();
        if s.len() != 17 && s.len() != 13 {
            bail!("an MLB is 17 (or 13) characters, not {s:?}");
        }
        id.mlb = s;
    }
    if let Some(u) = &a.uuid {
        id.uuid = smbios::parse_uuid(u)?;
    }
    if let Some(r) = &a.rom {
        id.rom = smbios::parse_rom(r)?;
    }
    let mut boot_args = a.boot_args.trim().to_string();
    if a.verbose_boot && !boot_args.split_whitespace().any(|w| w == "-v") {
        boot_args = format!("-v {boot_args}").trim().to_string();
    }
    let ovmf_dir = if a.no_ovmf {
        None
    } else {
        Some(a.ovmf_dir.clone().unwrap_or_else(|| {
            a.output
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("."))
        }))
    };
    Ok(opencore::Options {
        output: a.output.clone(),
        config: config::Options {
            smbios: id,
            timeout: a.timeout,
            boot_args,
            sip: !a.disable_sip,
            cpuid_spoof: !a.no_cpuid_spoof,
            hide_vm: !a.show_vm,
            graphical_picker: !a.text_picker,
            show_picker: a.show_picker,
            kexts: !a.no_kexts,
        },
        add_kexts: a.add_kext.clone(),
        ovmf_dir,
    })
}

pub fn print_opencore(out: &opencore::Outputs) {
    println!("{}", out.image.display());
    if let Some(o) = &out.ovmf {
        println!("{}", o.code.display());
        println!("{}", o.vars.display());
    }
    let s = &out.smbios;
    eprintln!(
        "\nSMBIOS: {}, serial {}, MLB {}, UUID {}, ROM {}",
        s.model, s.serial, s.mlb, s.uuid, s.rom
    );
}
