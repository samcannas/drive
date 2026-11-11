# Linux packaging notes

Drive shows the vault as a FUSE file system mounted at `~/Drive` (configurable with
`drive setup --folder`). Downloaded file contents are cached in `~/.cache/drive/content`.

## Runtime dependencies

* `fuse3` (provides `/usr/bin/fusermount3`, the setuid mount helper, and `/dev/fuse` access).
  Debian/Ubuntu: `fuse3`; Arch: `fuse3`; Fedora: `fuse3`.
* No libfuse headers or libraries are needed to build: the daemon uses the pure-Rust `fuser`
  crate and mounts through `fusermount3`.

## Files

| Source | Installed to |
| --- | --- |
| `target/release/drive` | `/usr/bin/drive` |
| `packaging/linux/drive.service` | `/usr/lib/systemd/user/drive.service` |

The unit is a *user* service. Packages should not enable it for all users; the app (or the
user) runs `systemctl --user enable --now drive.service`. After installing an upgrade, users
restart it with `systemctl --user restart drive.service`.

## Using it

```sh
drive setup --login you@example.com     # connects the account and mounts ~/Drive
drive status
setfattr -n user.drive.pinned -v 1 ~/Drive/Photos   # keep a folder downloaded
getfattr -n user.drive.status ~/Drive/Photos/a.jpg  # cloud-only | downloaded | pinned | syncing
drive free ~/Drive/Videos                           # drop downloaded copies
```

The mount point must be an empty folder: Drive refuses to mount over existing files. If the
daemon is killed, the next start cleans up the stale mount (or run `fusermount3 -uz ~/Drive`).
