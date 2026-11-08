//! OpenCore's `config.plist`, made from the `Sample.plist` of the release it boots with, so its
//! keys are always that release's.
//!
//! What changes from the sample is what a q35 VM on KVM needs, following the configuration the
//! reims-vgpu x86-64 guests were measured with (docs/qemu.md in reims-vgpu): an image built with
//! ULTMOS, whose kexts and settings for hardware a VM does not have (Bluetooth, audio codecs, GPU
//! power management, USB port maps, Intel graphics) are left out:
//! - the drivers a VM boots with: OpenRuntime, OpenHfsPlus (recovery images and installers are
//!   HFS+), OpenPartitionDxe (older recovery images' Apple partition maps) and ResetNvramEntry;
//! - the graphical picker, OpenCanopy, with the `Acidanthera\GoldenGate` icons (or the text
//!   picker, when asked for);
//! - the boot chime, which AudioDxe plays on the first Intel HDA controller and codec it finds,
//!   when the VM has one, unless macOS's "Play sound on startup" is off (`StartupMute`);
//! - on macOS 26, one kext, Lilu, which kexts given with `--add-kext` may link against; after it,
//!   those kexts (added.rs), in their order and on the Darwin versions given; no others, as QEMU's
//!   `isa-applesmc` is the SMC;
//! - one ACPI table, the `EC` and `USBX` devices macOS looks for (acpi.rs);
//! - CPUID leaf 1 reported as a Skylake-SP (family 6, model 85, stepping 4), a CPU every x86-64
//!   macOS from 10.13 to 26 knows, whatever `-cpu` model the VM has;
//! - the kernel quirks a hypervisor needs (`ProvideCurrentCpuInfo`, `DummyPowerManagement`,
//!   `ForceSecureBootScheme`), with Apple Secure Boot off, any volume scanned, no vault;
//! - optionally, the kernel's `kern.hv_vmm_present` swapped with `kern.hibernatecount`, so macOS
//!   does not see it runs in a VM and lets iCloud and the App Store sign in;
//! - the picker hidden, so OpenCore boots the default entry, unless asked to show it;
//! - the SMBIOS identity (smbios.rs), and the picker's timeout and boot arguments.

use super::added;
use super::smbios::Smbios;
use anyhow::{Context, Result, bail};
use plist::{Dictionary, Value};

/// The graphical picker's icon set, under `EFI\OC\Resources\Image`.
pub const PICKER_VARIANT: &str = "Acidanthera\\GoldenGate";

/// The drivers the image carries and loads, in order.
pub fn drivers(o: &Options) -> Vec<&'static str> {
    let mut d = vec!["OpenRuntime.efi", "OpenHfsPlus.efi"];
    if o.graphical_picker {
        d.push("OpenCanopy.efi");
    }
    d.extend([
        "AudioDxe.efi",
        "OpenPartitionDxe.efi",
        "ResetNvramEntry.efi",
    ]);
    d
}

/// A kext of reims-util's own in `EFI/OC/Kexts`: its bundle, its `CFBundleIdentifier`, and the
/// executable in its `Contents/MacOS`.
pub struct Kext {
    pub bundle: &'static str,
    pub identifier: &'static str,
    pub executable: &'static str,
}

pub const LILU: Kext = Kext {
    bundle: "Lilu.kext",
    identifier: "as.vit9696.Lilu",
    executable: "Lilu",
};

/// reims-util's kexts, in the order they load.
pub const OWN: [Kext; 1] = [LILU];

/// reims-util's kexts load on Darwin 25 alone, macOS 26; so does a kext added with no bounds of
/// its own.
pub const KEXT_MIN_KERNEL: &str = "25.0.0";
pub const KEXT_MAX_KERNEL: &str = "25.99.99";

/// Arguments for every boot: kernel symbols in panic logs, and no panics on a TLB shootdown or a
/// timeout a vCPU took because the host descheduled it.
pub const DEFAULT_BOOT_ARGS: &str = "keepsyms=1 tlbto_us=0 vti=9";

/// Apple's NVRAM variables' GUID.
const APPLE_NVRAM: &str = "7C436110-AB2A-4BBB-A880-FE41995C9F82";

#[derive(Clone, Debug)]
pub struct Options {
    pub smbios: Smbios,
    /// Seconds the picker waits before it boots the default entry; 0 waits for a choice.
    pub timeout: u32,
    pub boot_args: String,
    /// System Integrity Protection on (`csr-active-config` 0), or off (0x67, as the measured
    /// guests ran).
    pub sip: bool,
    pub cpuid_spoof: bool,
    pub hide_vm: bool,
    /// OpenCanopy's graphical picker, or OpenCore's text one.
    pub graphical_picker: bool,
    /// The picker at every boot, or none: OpenCore boots the default entry.
    pub show_picker: bool,
    /// Lilu, for macOS 26.
    pub kexts: bool,
}

/// The config.plist for `o`, from the release's `sample`. `added` (added.rs) loads after Lilu, or
/// alone when `o.kexts` leaves Lilu out.
pub fn make(sample: &[u8], o: &Options, added: &[added::Kext]) -> Result<Vec<u8>> {
    let mut root: Value = plist::from_bytes(sample).context("read Sample.plist")?;
    let c = root
        .as_dictionary_mut()
        .context("Sample.plist is not a dictionary")?;

    // ACPI: our table alone.
    let acpi_entry = {
        let mut e = template(c, &["ACPI", "Add"])?;
        e.insert(
            "Comment".into(),
            "EC and USBX devices macOS looks for".into(),
        );
        e.insert("Enabled".into(), true.into());
        e.insert("Path".into(), super::acpi::FILE_NAME.into());
        e
    };
    set(c, &["ACPI"], "Add", vec![acpi_entry.into()].into())?;
    set(c, &["ACPI"], "Delete", Value::Array(vec![]))?;
    set(c, &["ACPI"], "Patch", Value::Array(vec![]))?;

    set(c, &["Booter"], "MmioWhitelist", Value::Array(vec![]))?;
    set(c, &["Booter"], "Patch", Value::Array(vec![]))?;
    set(
        c,
        &["Booter", "Quirks"],
        "FixupAppleEfiImages",
        false.into(),
    )?;
    set(c, &["Booter", "Quirks"], "SetupVirtualMap", false.into())?;

    set(
        c,
        &["DeviceProperties"],
        "Add",
        Value::Dictionary(Dictionary::new()),
    )?;
    set(
        c,
        &["DeviceProperties"],
        "Delete",
        Value::Dictionary(Dictionary::new()),
    )?;

    let kexts = kernel_add(&template(c, &["Kernel", "Add"])?, o.kexts, added);
    set(c, &["Kernel"], "Add", Value::Array(kexts))?;
    set(c, &["Kernel"], "Block", Value::Array(vec![]))?;
    set(c, &["Kernel"], "Force", Value::Array(vec![]))?;
    let (data, mask) = if o.cpuid_spoof {
        (
            [0x54, 0x06, 0x05, 0x00, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0].to_vec(),
            [0xff, 0xff, 0xff, 0x00, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0].to_vec(),
        )
    } else {
        (vec![], vec![])
    };
    set(c, &["Kernel", "Emulate"], "Cpuid1Data", Value::Data(data))?;
    set(c, &["Kernel", "Emulate"], "Cpuid1Mask", Value::Data(mask))?;
    set(
        c,
        &["Kernel", "Emulate"],
        "DummyPowerManagement",
        true.into(),
    )?;
    let mut patches = Vec::new();
    if o.hide_vm {
        for (comment, find, replace) in [
            (
                "Hide the VM: kern.hibernatecount reads kern.hv_vmm_present (1 of 2)",
                &b"hibernatehidready\0hibernatecount\0"[..],
                &b"hibernatehidready\0hv_vmm_present\0"[..],
            ),
            (
                "Hide the VM: kern.hv_vmm_present reads kern.hibernatecount (2 of 2)",
                &b"boot session UUID\0hv_vmm_present\0"[..],
                &b"boot session UUID\0hibernatecount\0"[..],
            ),
        ] {
            let mut p = template(c, &["Kernel", "Patch"])?;
            p.insert("Arch".into(), "x86_64".into());
            p.insert("Base".into(), "".into());
            p.insert("Comment".into(), comment.into());
            p.insert("Count".into(), 1.into());
            p.insert("Enabled".into(), true.into());
            p.insert("Find".into(), Value::Data(find.to_vec()));
            p.insert("Identifier".into(), "kernel".into());
            p.insert("Limit".into(), 0.into());
            p.insert("Mask".into(), Value::Data(vec![]));
            p.insert("MaxKernel".into(), "".into());
            p.insert("MinKernel".into(), "".into());
            p.insert("Replace".into(), Value::Data(replace.to_vec()));
            p.insert("ReplaceMask".into(), Value::Data(vec![]));
            p.insert("Skip".into(), 0.into());
            patches.push(Value::Dictionary(p));
        }
    }
    set(c, &["Kernel"], "Patch", Value::Array(patches))?;
    set(
        c,
        &["Kernel", "Quirks"],
        "ForceSecureBootScheme",
        true.into(),
    )?;
    set(
        c,
        &["Kernel", "Quirks"],
        "ProvideCurrentCpuInfo",
        true.into(),
    )?;
    set(c, &["Kernel", "Quirks"], "SetApfsTrimTimeout", 0.into())?;

    set(c, &["Misc", "Boot"], "HideAuxiliary", false.into())?;
    let mode = if o.graphical_picker {
        "External"
    } else {
        "Builtin"
    };
    set(c, &["Misc", "Boot"], "PickerMode", mode.into())?;
    set(c, &["Misc", "Boot"], "PickerVariant", PICKER_VARIANT.into())?;
    set(c, &["Misc", "Boot"], "PollAppleHotKeys", true.into())?;
    set(c, &["Misc", "Boot"], "ShowPicker", o.show_picker.into())?;
    set(c, &["Misc", "Boot"], "Timeout", (o.timeout as i64).into())?;
    set(c, &["Misc"], "Entries", Value::Array(vec![]))?;
    set(c, &["Misc"], "Tools", Value::Array(vec![]))?;
    set(c, &["Misc", "Security"], "AllowSetDefault", true.into())?;
    set(c, &["Misc", "Security"], "ScanPolicy", 0.into())?;
    set(
        c,
        &["Misc", "Security"],
        "SecureBootModel",
        "Disabled".into(),
    )?;
    set(c, &["Misc", "Security"], "Vault", "Optional".into())?;

    let nvram = &["NVRAM", "Add", APPLE_NVRAM];
    set(c, nvram, "boot-args", o.boot_args.trim().into())?;
    let csr: u32 = if o.sip { 0 } else { 0x67 };
    set(
        c,
        nvram,
        "csr-active-config",
        Value::Data(csr.to_le_bytes().to_vec()),
    )?;
    set(c, nvram, "prev-lang:kbd", Value::Data(b"en-US:0".to_vec()))?;
    // Deleted before they are added, so the image's values win over what NVRAM holds.
    let delete = dict(c, &["NVRAM", "Delete"])?;
    let mut names: Vec<Value> = match delete.get(APPLE_NVRAM) {
        Some(Value::Array(a)) => a.clone(),
        _ => vec![],
    };
    for n in ["boot-args", "csr-active-config", "prev-lang:kbd"] {
        if !names.iter().any(|v| v.as_string() == Some(n)) {
            names.push(n.into());
        }
    }
    delete.insert(APPLE_NVRAM.into(), Value::Array(names));

    let g = &["PlatformInfo", "Generic"];
    set(c, &["PlatformInfo"], "Automatic", true.into())?;
    set(c, g, "SystemProductName", o.smbios.model.as_str().into())?;
    set(c, g, "SystemSerialNumber", o.smbios.serial.as_str().into())?;
    set(c, g, "MLB", o.smbios.mlb.as_str().into())?;
    set(c, g, "SystemUUID", o.smbios.uuid.as_str().into())?;
    set(c, g, "ROM", Value::Data(unhex(&o.smbios.rom)?))?;

    set(c, &["UEFI", "APFS"], "MinDate", (-1i64).into())?;
    set(c, &["UEFI", "APFS"], "MinVersion", (-1i64).into())?;
    // The chime: an empty device path is the first HDA controller and codec AudioDxe finds, and a
    // mask of -1 every output of that codec. Auto stays silent when `StartupMute` is set.
    set(c, &["UEFI", "Audio"], "AudioSupport", true.into())?;
    set(c, &["UEFI", "Audio"], "AudioDevice", "".into())?;
    set(c, &["UEFI", "Audio"], "AudioOutMask", (-1i64).into())?;
    set(c, &["UEFI", "Audio"], "PlayChime", "Auto".into())?;
    set(c, &["UEFI", "Output"], "Resolution", "".into())?;
    let sample_drivers: Vec<Value> = match dict(c, &["UEFI"])?.get("Drivers") {
        Some(Value::Array(a)) => a.clone(),
        _ => bail!("Sample.plist has no UEFI > Drivers"),
    };
    let mut entries = Vec::new();
    for name in drivers(o) {
        let mut e = sample_drivers
            .iter()
            .filter_map(Value::as_dictionary)
            .find(|d| d.get("Path").and_then(Value::as_string) == Some(name))
            .cloned()
            .or_else(|| {
                sample_drivers
                    .first()
                    .and_then(Value::as_dictionary)
                    .cloned()
            })
            .context("Sample.plist lists no drivers")?;
        e.insert("Path".into(), name.into());
        e.insert("Arguments".into(), "".into());
        e.insert("Enabled".into(), true.into());
        e.insert("LoadEarly".into(), false.into());
        entries.push(Value::Dictionary(e));
    }
    set(c, &["UEFI"], "Drivers", Value::Array(entries))?;
    set(c, &["UEFI"], "ReservedMemory", Value::Array(vec![]))?;

    let mut out = Vec::new();
    root.to_writer_xml(&mut out).context("write config.plist")?;
    Ok(out)
}

/// Kernel > Add's entries, made from the sample's first: with `own`, Lilu for macOS 26, then
/// `added`, in its order, on the Darwin versions each was given.
fn kernel_add(template: &Dictionary, own: bool, added: &[added::Kext]) -> Vec<Value> {
    let mut add = Vec::new();
    if own {
        add.push((
            LILU.bundle,
            Some(LILU.executable),
            KEXT_MIN_KERNEL,
            KEXT_MAX_KERNEL,
            "Patch engine",
        ));
    }
    for k in added {
        add.push((
            &k.bundle,
            k.executable.as_deref(),
            &k.min_kernel,
            &k.max_kernel,
            &k.identifier,
        ));
    }
    add.into_iter()
        .map(|(bundle, executable, min_kernel, max_kernel, comment)| {
            let mut e = template.clone();
            e.insert("Arch".into(), "x86_64".into());
            e.insert("BundlePath".into(), bundle.into());
            e.insert("Comment".into(), comment.into());
            e.insert("Enabled".into(), true.into());
            // A kext with no code has an empty path.
            let path = executable.map(|x| format!("Contents/MacOS/{x}"));
            e.insert("ExecutablePath".into(), path.unwrap_or_default().into());
            e.insert("MaxKernel".into(), max_kernel.into());
            e.insert("MinKernel".into(), min_kernel.into());
            e.insert("PlistPath".into(), "Contents/Info.plist".into());
            Value::Dictionary(e)
        })
        .collect()
}

fn dict<'a>(root: &'a mut Dictionary, path: &[&str]) -> Result<&'a mut Dictionary> {
    let mut d = root;
    for (i, k) in path.iter().enumerate() {
        d = d
            .get_mut(k)
            .and_then(Value::as_dictionary_mut)
            .with_context(|| format!("Sample.plist has no {}", path[..=i].join(" > ")))?;
    }
    Ok(d)
}

/// Sets a key that the sample has, so a key a later release renamed fails here, not at boot.
fn set(root: &mut Dictionary, path: &[&str], key: &str, value: Value) -> Result<()> {
    let d = dict(root, path)?;
    if !d.contains_key(key) {
        bail!("Sample.plist has no {} > {key}", path.join(" > "));
    }
    d.insert(key.into(), value);
    Ok(())
}

/// The first entry of an array of dictionaries: a new entry has its keys.
fn template(root: &mut Dictionary, path: &[&str]) -> Result<Dictionary> {
    let (parent, key) = path.split_at(path.len() - 1);
    let d = dict(root, parent)?;
    d.get(key[0])
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(Value::as_dictionary)
        .cloned()
        .with_context(|| format!("Sample.plist has no entry in {}", path.join(" > ")))
}

fn unhex(s: &str) -> Result<Vec<u8>> {
    (0..s.len())
        .step_by(2)
        .map(|i| {
            s.get(i..i + 2)
                .and_then(|b| u8::from_str_radix(b, 16).ok())
                .with_context(|| format!("not hex: {s}"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields(v: &Value, keys: &[&str]) -> Vec<String> {
        let d = v.as_dictionary().unwrap();
        keys.iter()
            .map(|k| d.get(k).and_then(Value::as_string).unwrap().to_string())
            .collect()
    }

    fn added(bundle: &str, executable: Option<&str>, min: &str, max: &str) -> added::Kext {
        let plist = added::tests::info(&format!("t.{bundle}"), executable, &[]);
        let mut files: Vec<(&str, &[u8])> = vec![("Contents/Info.plist", &plist)];
        let path = executable.map(|x| format!("Contents/MacOS/{x}"));
        if let Some(p) = &path {
            files.push((p, &[0xcf]));
        }
        let b = added::tests::Bundle::new(&format!("{bundle}.kext"), &files);
        let s = added::parse(&format!("{},{min},{max}", b.path.display())).unwrap();
        added::read(&s).unwrap()
    }

    #[test]
    fn kernel_add_entries() {
        let mut template = Dictionary::new();
        template.insert("BundlePath".into(), "Lilu.kext".into());
        template.insert("Enabled".into(), false.into());
        let keys = [
            "BundlePath",
            "ExecutablePath",
            "PlistPath",
            "MinKernel",
            "MaxKernel",
            "Arch",
        ];
        let want = |b: &str, x: &str, min: &str, max: &str| {
            [b, x, "Contents/Info.plist", min, max, "x86_64"].map(str::to_string)
        };
        let entries = |own: bool, added: &[added::Kext]| -> Vec<[String; 6]> {
            kernel_add(&template, own, added)
                .iter()
                .map(|e| {
                    assert_eq!(e.as_dictionary().unwrap()["Enabled"], Value::Boolean(true));
                    fields(e, &keys).try_into().unwrap()
                })
                .collect()
        };
        let lilu = want(
            "Lilu.kext",
            "Contents/MacOS/Lilu",
            KEXT_MIN_KERNEL,
            KEXT_MAX_KERNEL,
        );

        // Lilu alone, or nothing.
        assert_eq!(entries(true, &[]), std::slice::from_ref(&lilu));
        assert!(entries(false, &[]).is_empty());

        // Added kexts follow, in their order, on their own bounds; one with no code has no path.
        let set = [
            added("Loader", Some("Loader"), "25.4.0", "25.99.99"),
            added("Shim", None, "25.4.0", "25.6.0"),
            added("Gfx", Some("ReimsGraphics"), "25.0.0", "25.99.99"),
        ];
        let tail = [
            want("Loader.kext", "Contents/MacOS/Loader", "25.4.0", "25.99.99"),
            want("Shim.kext", "", "25.4.0", "25.6.0"),
            want(
                "Gfx.kext",
                "Contents/MacOS/ReimsGraphics",
                "25.0.0",
                "25.99.99",
            ),
        ];
        let mut all = vec![lilu];
        all.extend(tail.clone());
        assert_eq!(entries(true, &set), all);
        assert_eq!(entries(false, &set), tail);
        let e = &kernel_add(&template, true, &set)[1];
        assert_eq!(fields(e, &["Comment"]), ["t.Loader"]);
    }
}
