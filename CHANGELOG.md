# Changelog

## Unreleased

- OVMF's package, `edk2-ovmf-202608-1-any.pkg.tar.zst`, has a second source: the Arch Linux
  Archive's mirror at `umea.archive.pkgbuild.com`, tried when `archive.archlinux.org` fails. On
  2026-11-21 the archive answered nothing over IPv4 and a fresh install made no OVMF; the mirror
  served the same file, with the pinned SHA-256. The first source now gives up after 2 failed
  requests in a row instead of 10, so a dead host delays the download by about a minute and a
  half.
- When every source of the package fails, the error names each source and what it answered, and
  says where to put the file by hand: in `~/.cache/reims-util`, where it is used if its SHA-256
  matches the pin.
