//! Kexts given with `--add-kext`, which the image carries in the order given: reims-vgpu's guest
//! kexts for its own GPU device, for one. Each is a bundle on disk, copied
//! whole into `EFI/OC/Kexts` (dotfiles left out), and loaded by a Kernel > Add entry made from its
//! Info.plist and the Darwin versions it was given (config.rs).

use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};

/// A `--add-kext` value, `PATH[,MIN[,MAX]]`: a bundle, and the Darwin versions it loads on, each
/// bound empty for none.
#[derive(Clone, Debug, PartialEq)]
pub struct Spec {
    pub path: PathBuf,
    pub min_kernel: String,
    pub max_kernel: String,
}

/// Reads `PATH[,MIN[,MAX]]`. The path ends in `.kext`, so a comma in a folder's name stays in
/// the path; MIN and MAX are versions as OpenCore takes them (25.4.0), and one left out or empty
/// is no bound, as OpenCore takes an empty MinKernel or MaxKernel.
pub fn parse(s: &str) -> Result<Spec, String> {
    let usage = "give PATH[,MIN[,MAX]], such as Foo.kext,25.4.0,25.99.99";
    let mut path = s;
    let mut bounds: Vec<&str> = Vec::new();
    while !is_bundle(path) && bounds.len() < 2 {
        let Some((p, b)) = path.rsplit_once(',') else {
            break;
        };
        path = p;
        bounds.insert(0, b);
    }
    if !is_bundle(path) {
        return Err(format!("{s:?} is not a .kext bundle: {usage}"));
    }
    let bound = |i: usize| -> Result<String, String> {
        match bounds.get(i).map(|b| b.trim()).filter(|b| !b.is_empty()) {
            None => Ok(String::new()),
            Some(b) if version(b).is_some() => Ok(b.to_string()),
            Some(b) => Err(format!(
                "{b:?} is not a Darwin version such as 25.4.0: {usage}"
            )),
        }
    };
    let min_kernel = bound(0)?;
    let max_kernel = bound(1)?;
    if let (Some(lo), Some(hi)) = (version(&min_kernel), version(&max_kernel))
        && lo > hi
    {
        return Err(format!(
            "{s:?} loads on no Darwin version: {min_kernel} is past {max_kernel}"
        ));
    }
    Ok(Spec {
        path: PathBuf::from(path),
        min_kernel,
        max_kernel,
    })
}

fn is_bundle(path: &str) -> bool {
    path.trim_end_matches('/').ends_with(".kext")
}

/// A version as OpenCore's MinKernel and MaxKernel take it, X.Y.Z.
fn version(s: &str) -> Option<[u32; 3]> {
    let mut parts = s.split('.').map(|p| {
        (!p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
            .then(|| p.parse().ok())
            .flatten()
    });
    let v = [parts.next()??, parts.next()??, parts.next()??];
    parts.next().is_none().then_some(v)
}

/// A kext to add, read from its bundle.
#[derive(Debug)]
pub struct Kext {
    /// The bundle's name, which it has in `EFI/OC/Kexts`.
    pub bundle: String,
    pub identifier: String,
    /// Its executable in `Contents/MacOS`, or None for a kext with no code.
    pub executable: Option<String>,
    /// The bundles its `OSBundleLibraries` name.
    libraries: Vec<String>,
    pub min_kernel: String,
    pub max_kernel: String,
    /// Its files, by their path in the bundle.
    pub files: Vec<(String, Vec<u8>)>,
}

/// Reads the bundle `s` names.
pub fn read(s: &Spec) -> Result<Kext> {
    let shown = s.path.display();
    let bundle = s
        .path
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| n.ends_with(".kext"))
        .with_context(|| format!("{shown} is not a .kext bundle"))?
        .to_string();
    // A link to a bundle is followed, as macOS follows it; links within one are refused.
    let root = std::fs::canonicalize(&s.path).with_context(|| format!("read {shown}"))?;
    if !root.is_dir() {
        bail!("{shown} is not a bundle: it is a file, not a folder");
    }
    let mut files = Vec::new();
    walk(&root, Path::new(""), &mut files)?;
    files.sort();
    let plist = files
        .iter()
        .find(|(p, _)| p == "Contents/Info.plist")
        .with_context(|| format!("{shown} has no Contents/Info.plist"))?;
    let info: plist::Dictionary =
        plist::from_bytes(&plist.1).with_context(|| format!("read {shown}/Contents/Info.plist"))?;
    let string = |k: &str| info.get(k).and_then(plist::Value::as_string);
    let identifier = string("CFBundleIdentifier")
        .filter(|i| !i.is_empty())
        .with_context(|| format!("{shown} has no CFBundleIdentifier"))?
        .to_string();
    let executable = string("CFBundleExecutable")
        .filter(|e| !e.is_empty())
        .map(str::to_string);
    if let Some(e) = &executable {
        let path = format!("Contents/MacOS/{e}");
        if !files.iter().any(|(p, _)| *p == path) {
            bail!("{shown} has no {path}, the executable its Info.plist names");
        }
    }
    let mut libraries: Vec<String> = ["OSBundleLibraries", "OSBundleLibraries_x86_64"]
        .iter()
        .filter_map(|k| info.get(k).and_then(plist::Value::as_dictionary))
        .flat_map(|d| d.keys().cloned())
        .collect();
    libraries.sort();
    libraries.dedup();
    Ok(Kext {
        bundle,
        identifier,
        executable,
        libraries,
        min_kernel: s.min_kernel.clone(),
        max_kernel: s.max_kernel.clone(),
        files,
    })
}

/// Adds the files under `root/rel`, by their path under `root`, skipping dotfiles.
fn walk(root: &Path, rel: &Path, files: &mut Vec<(String, Vec<u8>)>) -> Result<()> {
    let path = root.join(rel);
    let t = std::fs::symlink_metadata(&path)
        .with_context(|| format!("read {}", path.display()))?
        .file_type();
    if t.is_dir() {
        let mut entries = std::fs::read_dir(&path)
            .and_then(|r| r.collect::<Result<Vec<_>, _>>())
            .with_context(|| format!("read {}", path.display()))?;
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            if !e.file_name().to_string_lossy().starts_with('.') {
                walk(root, &rel.join(e.file_name()), files)?;
            }
        }
    } else if t.is_file() {
        let name = rel
            .to_str()
            .with_context(|| format!("{} is not UTF-8", path.display()))?
            .replace('\\', "/");
        let data = std::fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        files.push((name, data));
    } else {
        bail!(
            "{} is neither a file nor a folder (a link?): give a bundle of files",
            path.display()
        );
    }
    Ok(())
}

/// Refuses `added` where its kexts would clash with each other: a bundle name FAT32 can't tell
/// from another's (it ignores case), an identifier given twice, or a kext given before one it
/// links against.
pub fn check(added: &[Kext]) -> Result<()> {
    for (i, k) in added.iter().enumerate() {
        let earlier = &added[..i];
        let same = |b: &str| b.eq_ignore_ascii_case(&k.bundle);
        if let Some(e) = earlier.iter().find(|e| same(&e.bundle)) {
            if e.bundle == k.bundle {
                bail!("{} is given twice", k.bundle);
            }
            bail!(
                "{} and {} are one name on the image's FAT32 partition, which ignores case",
                e.bundle,
                k.bundle
            );
        }
        if let Some(e) = earlier.iter().find(|e| e.identifier == k.identifier) {
            bail!("{} and {} are both {}", e.bundle, k.bundle, k.identifier);
        }
        for l in &k.libraries {
            if *l == k.identifier || earlier.iter().any(|e| e.identifier == *l) {
                continue;
            }
            if let Some(later) = added[i + 1..].iter().find(|a| a.identifier == *l) {
                bail!(
                    "{} links against {} ({l}), which comes after it: give {} first",
                    k.bundle,
                    later.bundle,
                    later.bundle
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn spec(path: &str, min: &str, max: &str) -> Spec {
        Spec {
            path: PathBuf::from(path),
            min_kernel: min.into(),
            max_kernel: max.into(),
        }
    }

    #[test]
    fn parsed() {
        let (lo, hi) = ("", "");
        for (s, want) in [
            ("Foo.kext", spec("Foo.kext", lo, hi)),
            ("/k/Foo.kext/", spec("/k/Foo.kext/", lo, hi)),
            ("Foo.kext,25.4.0", spec("Foo.kext", "25.4.0", hi)),
            (
                "/k/Foo.kext,25.4.0,25.6.0",
                spec("/k/Foo.kext", "25.4.0", "25.6.0"),
            ),
            ("Foo.kext,,25.6.0", spec("Foo.kext", lo, "25.6.0")),
            ("Foo.kext, 25.4.0 ,", spec("Foo.kext", "25.4.0", hi)),
            ("/a,b/Foo.kext", spec("/a,b/Foo.kext", lo, hi)),
            (
                "/a,b/Foo.kext,25.4.0,25.99.99",
                spec("/a,b/Foo.kext", "25.4.0", "25.99.99"),
            ),
            ("Foo.kext,9.0.0", spec("Foo.kext", "9.0.0", hi)),
            ("Foo.kext,26.0.0", spec("Foo.kext", "26.0.0", hi)),
        ] {
            assert_eq!(parse(s), Ok(want), "{s}");
        }
    }

    #[test]
    fn refused() {
        for (s, says) in [
            ("Foo", "not a .kext bundle"),
            ("Foo.kext,25.4.0,25.99.99,1", "not a .kext bundle"),
            ("Foo.kext,25.4", "not a Darwin version"),
            ("Foo.kext,25.4.0.1", "not a Darwin version"),
            ("Foo.kext,25.x.0", "not a Darwin version"),
            ("Foo.kext,+25.4.0", "not a Darwin version"),
            ("Foo.kext,25.6.0,25.4.0", "past 25.4.0"),
        ] {
            let e = parse(s).unwrap_err();
            assert!(e.contains(says), "{s}: {e}");
        }
    }

    /// A bundle in a new folder under the system's temporary one, with `files` by their path
    /// in it; removed when dropped.
    pub(crate) struct Bundle {
        pub path: PathBuf,
        dir: PathBuf,
    }

    impl Bundle {
        pub(crate) fn new(name: &str, files: &[(&str, &[u8])]) -> Bundle {
            let mut tag = [0u8; 8];
            getrandom::fill(&mut tag).unwrap();
            let tag: String = tag.iter().map(|b| format!("{b:02x}")).collect();
            let dir = std::env::temp_dir().join(format!("reims-util-test-{tag}"));
            let path = dir.join(name);
            for (p, d) in files {
                let f = path.join(p);
                std::fs::create_dir_all(f.parent().unwrap()).unwrap();
                std::fs::write(f, d).unwrap();
            }
            std::fs::create_dir_all(&path).unwrap();
            Bundle { path, dir }
        }

        pub(crate) fn spec(&self) -> Spec {
            parse(self.path.to_str().unwrap()).unwrap()
        }
    }

    impl Drop for Bundle {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// An Info.plist: its identifier, executable and libraries.
    pub(crate) fn info(id: &str, executable: Option<&str>, libraries: &[&str]) -> Vec<u8> {
        let mut d = plist::Dictionary::new();
        d.insert("CFBundleIdentifier".into(), id.into());
        if let Some(e) = executable {
            d.insert("CFBundleExecutable".into(), e.into());
        }
        let mut l = plist::Dictionary::new();
        for x in libraries {
            l.insert(x.to_string(), "1.0.0".into());
        }
        d.insert("OSBundleLibraries".into(), l.into());
        let mut v = Vec::new();
        plist::Value::Dictionary(d).to_writer_xml(&mut v).unwrap();
        v
    }

    /// A kext as `read` gives it, for `check`.
    fn kext(bundle: &str, id: &str, libraries: &[&str]) -> Kext {
        Kext {
            bundle: bundle.into(),
            identifier: id.into(),
            executable: None,
            libraries: libraries.iter().map(|l| l.to_string()).collect(),
            min_kernel: String::new(),
            max_kernel: String::new(),
            files: Vec::new(),
        }
    }

    #[test]
    fn read_with_code() {
        let plist = info("t.loader", Some("Loader"), &["com.apple.kpi.libkern"]);
        let b = Bundle::new(
            "Loader.kext",
            &[
                ("Contents/Info.plist", &plist),
                ("Contents/MacOS/Loader", &[0xcf]),
                ("Contents/Resources/en.lproj/x.strings", b"x"),
                (".DS_Store", b"junk"),
                ("Contents/._Info.plist", b"junk"),
            ],
        );
        let s = parse(&format!("{},25.4.0", b.path.display())).unwrap();
        let k = read(&s).unwrap();
        assert_eq!(k.bundle, "Loader.kext");
        assert_eq!(k.identifier, "t.loader");
        assert_eq!(k.executable.as_deref(), Some("Loader"));
        assert_eq!(k.libraries, ["com.apple.kpi.libkern"]);
        assert_eq!(
            (k.min_kernel.as_str(), k.max_kernel.as_str()),
            ("25.4.0", "")
        );
        let paths: Vec<&str> = k.files.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(
            paths,
            [
                "Contents/Info.plist",
                "Contents/MacOS/Loader",
                "Contents/Resources/en.lproj/x.strings"
            ]
        );
    }

    #[test]
    fn read_codeless() {
        let plist = info("t.shim", None, &[]);
        let b = Bundle::new("Shim.kext", &[("Contents/Info.plist", &plist)]);
        let k = read(&b.spec()).unwrap();
        assert_eq!(k.executable, None);
        assert_eq!(k.files.len(), 1);
    }

    #[test]
    fn read_refused() {
        let plist = info("t.x", Some("X"), &[]);
        let b = Bundle::new("X.kext", &[("Contents/Info.plist", &plist)]);
        let e = format!("{:#}", read(&b.spec()).unwrap_err());
        assert!(e.contains("no Contents/MacOS/X"), "{e}");

        let b = Bundle::new("Y.kext", &[("Contents/MacOS/Y", &[0xcf])]);
        let e = format!("{:#}", read(&b.spec()).unwrap_err());
        assert!(e.contains("no Contents/Info.plist"), "{e}");

        let b = Bundle::new(
            "Z.kext",
            &[("Contents/Info.plist", &info("", None, &[])[..])],
        );
        let e = format!("{:#}", read(&b.spec()).unwrap_err());
        assert!(e.contains("no CFBundleIdentifier"), "{e}");
        let mut d = plist::Dictionary::new();
        d.insert("CFBundleExecutable".into(), "Z".into());
        let mut v = Vec::new();
        plist::Value::Dictionary(d).to_writer_xml(&mut v).unwrap();
        let b = Bundle::new("Z.kext", &[("Contents/Info.plist", &v)]);
        let e = format!("{:#}", read(&b.spec()).unwrap_err());
        assert!(e.contains("no CFBundleIdentifier"), "{e}");

        let e = format!(
            "{:#}",
            read(&spec("/nonexistent/N.kext", "1.0.0", "1.0.0")).unwrap_err()
        );
        assert!(e.contains("read /nonexistent/N.kext"), "{e}");
    }

    #[test]
    fn checked() {
        let ok = [
            kext("Loader.kext", "t.loader", &["com.apple.kpi.libkern"]),
            kext("Shim.kext", "t.shim", &["com.apple.kpi.iokit"]),
            kext("Gfx.kext", "t.gfx", &["t.shim", "t.loader", "t.gfx"]),
        ];
        check(&ok).unwrap();
        check(&[]).unwrap();

        let refused = |added: &[Kext], says: &str| {
            let e = check(added).unwrap_err().to_string();
            assert!(e.contains(says), "{says}: {e}");
        };
        refused(
            &[kext("A.kext", "t.a", &[]), kext("A.kext", "t.b", &[])],
            "A.kext is given twice",
        );
        refused(
            &[kext("A.kext", "t.a", &[]), kext("a.kext", "t.b", &[])],
            "A.kext and a.kext are one name",
        );
        refused(
            &[kext("A.kext", "t.a", &[]), kext("B.kext", "t.a", &[])],
            "A.kext and B.kext are both t.a",
        );
        refused(
            &[kext("A.kext", "t.a", &["t.b"]), kext("B.kext", "t.b", &[])],
            "A.kext links against B.kext (t.b), which comes after it: give B.kext first",
        );
    }
}
