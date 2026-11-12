# Drive

An iCloud-style cloud drive for Windows and Linux, backed by your [uloz.to](https://uloz.to)
account.

- **Files live in the cloud, not on your disk.** Everything in your Drive folder shows up
  with its real name and size. A file downloads only when you open it, and the least
  recently used copies are cleared out automatically once the cache limit is reached.
- **Set and forget.** Anything you put in the folder uploads in the background. A tray icon
  shows what's happening and only interrupts you when something needs a decision.
- **End-to-end encrypted.** Names and contents are encrypted on your device before upload,
  in the same format as [`rclone crypt`](https://rclone.org/crypt/). uloz.to only ever sees
  encrypted blobs, and with the recovery kit you can always read your data with plain
  rclone, even without Drive.
- **Native integration.**
  - **Windows:** a Cloud Files sync root, like OneDrive and iCloud. You get Explorer status
    icons, *Always keep on this device* and *Free up space*.
  - **Linux:** a FUSE mount at `~/Drive` with an on-demand cache. Pin files with
    `setfattr -n user.drive.pinned -v 1 FILE`.

## Install

Download the latest release from
[Releases](https://github.com/samcannas/drive/releases/latest):

| System | File | Install |
|---|---|---|
| Windows 10/11 | `Drive_<version>_x64-setup.exe` | Run it. It installs for your user only, no admin needed. It isn't code-signed yet, so on the SmartScreen warning choose **More info → Run anyway**. |
| Ubuntu / Debian | `Drive_<version>_amd64.deb` | `sudo apt install ./Drive_<version>_amd64.deb` then `systemctl --user enable --now drive` |
| Arch / Manjaro / Omarchy | `drive-<version>-1-x86_64.pkg.tar.zst` | `sudo pacman -U drive-<version>-1-x86_64.pkg.tar.zst` then `systemctl --user enable --now drive` |
| Any Linux | `Drive_<version>_amd64.AppImage` | `chmod +x` it and run it (needs `fuse3`) |

Start **Drive** from the Start menu or your app launcher. On first run it walks you through
signing in to uloz.to and choosing where your Drive folder goes (default `~/Drive`).

> **Your encryption password.** For a new vault, Drive generates a strong password and
> shows it **once**. Save it in a password manager, and also use *Save recovery kit…*.
> You need the password to set up Drive on another device, and nobody, uloz.to included,
> can recover your files without it.

## Everyday use

| You want to… | Windows | Linux |
|---|---|---|
| Keep something offline | Right-click → **Always keep on this device** | `drive pin PATH` or `setfattr -n user.drive.pinned -v 1 PATH` |
| Free up space | Right-click → **Free up space** | `drive free PATH` |
| See what's happening | Click the tray icon | Tray menu → *Show status*, or `drive status` |
| Pause syncing | Tray menu → **Pause syncing** | `drive pause` / `drive resume` |

The `drive` command-line tool works on both systems: `drive status`, `drive activity`,
`drive issues`, `drive settings cache-limit-gb=20`, `drive recovery`, `drive sign-out`, …
Run `drive --help` for the list.

### What happens when…

- **You edit the same file on two devices at once:** both versions are kept. One is renamed
  `name (conflict DEVICE DATE).ext` and you get a notice.
- **Lots of files get deleted at once** (over 500 locally, or most of the vault remotely):
  Drive asks before deleting anything else. Deleted files stay in the uloz.to trash for 30
  days.
- **You're offline:** files you've kept on the device still open, changes queue up, and
  everything syncs when you're back.
- **Your disk fills up:** cached copies are cleared first. Files you've pinned are never
  removed.

## Transfers

1. **See what's going on.** The tray shows what's uploading or downloading and how much is
   left, like *Uploading 3 files · 45 MB of 1.6 GB*.
2. **Know what to expect.** Each file shows its own progress and time left. If something is
   waiting, it tells you why, and asks you to keep the PC on for long uploads.
3. **Stay in control.** Pause, resume or upload any file right away, or hold big ones for
   tonight so they don't slow your day.
4. **Let Drive handle it.** Big files upload in checked pieces and pick up where they left
   off after a restart or a dropped connection. If uploads make your internet sluggish, Drive
   slows them down until it's smooth again, and only then.

## Back up other folders

Drive can also protect folders outside the Drive folder, like Desktop, Documents or Downloads.
They stay where they are; Drive only keeps an encrypted copy in your vault.

1. **See what's protected.** Settings → Backups shows each backed-up folder, how much of it is
   in the cloud, and when it was last fully backed up.
2. **Get a suggestion.** Drive notices your Desktop, Documents, Downloads, Pictures, Videos and
   Music folders and tells you which aren't backed up yet and how big they are.
3. **Back up with one click.** Pick a folder and Drive keeps a copy up to date as you work. It
   never moves, changes or deletes your files, and each folder has an off switch.

**Drive - Backups.** Next to your Drive folder, *Drive - Backups* (also in Explorer's sidebar)
holds a link to each backed-up folder. Opening one opens the real folder, so it's fully
usable and takes no extra space. The Drive folder itself stays just your files.

**Save space (Windows).** Turn on *Save space on this PC* for a backed-up folder, and it
becomes on-demand like the Drive folder: it stays where it is, but files you haven't opened
lately live only in the cloud and download when you open them. Turning it off downloads
everything back first.

Build folders like `node_modules` are skipped. Deleted files stay in the uloz.to trash for 30
days. From the command line: `drive backup list`, `drive backup add <folder>`,
`drive backup save-space <folder> on|off`.

## Free up disk space

**Settings → Disk space** shows where the space on your disk goes, not just what's in Drive:

1. **See it.** Your disk's free space, what Drive already keeps off this PC, and the biggest
   uses of space: your folders, installed apps, app data, the Recycle Bin, Windows itself.
   The tray popover also shows how much is free and speaks up, quietly, when the disk is
   almost full.
2. **Get a recommendation.** For each of your own folders Drive says how much it could free.
   Folders it shouldn't touch (apps, app data, code projects, other cloud apps' folders) say
   why and who manages them.
3. **Free it, with your OK.** *Save space* backs the folder up and makes it on-demand in one
   step (Windows). Nothing moves and nothing is deleted. Files you haven't opened lately live
   only in the cloud once they're backed up, and download again when you open them. For the
   Drive folder, *Free up space* drops downloaded files (pinned ones stay). Switch back any
   time in Settings → Backups.

Drive already frees space by itself in the Drive folder when its cache is full or the disk runs
low. From the command line: `drive space`, `drive space scan`, `drive space save <folder>`,
`drive space free-drive`. On Linux, Drive measures the disk and backs folders up; saving space
in place is Windows-only for now.

## uloz.to notes

- Downloading through the API needs a uloz.to account that allows fast downloads (a paid
  plan). Free accounts are limited to slow downloads and captchas.
- Drive uses uloz.to's shared testing API key by default. uloz.to asks real apps to request
  a dedicated key from their support. If they give you one, enter it in **Settings → Account**.
- Your files are stored in a folder called `Drive vault` in your uloz.to account. Leave it
  alone in the uloz.to web interface: everything inside is encrypted.

## Getting your data out without Drive

**Settings → Recovery → Export recovery kit** (or `drive recovery`) produces an
`rclone.conf`. With [rclone](https://rclone.org) installed:

```sh
rclone --config rclone.conf config update uloz password YOUR_ULOZTO_PASSWORD --obscure
rclone --config rclone.conf copy drive: ./restored
```

## Development

```sh
cargo test -p drive-core -p drive-daemon     # engine, crypto, platform integration tests
cd app && pnpm install && pnpm dev           # UI in the browser with mock data
```

Run the whole stack locally without a uloz.to account by pointing the service at a local
folder:

```sh
DRIVE_HOME=/tmp/drive-home DRIVE_DEV_REMOTE=/tmp/drive-cloud drive daemon --foreground
drive setup --login dev --folder ~/DriveTest
```

More detail: [docs/architecture.md](docs/architecture.md),
[docs/database.md](docs/database.md), [app/README.md](app/README.md).

Every push to `main` builds the Windows installer, `.deb`, AppImage and Arch package and
publishes them as a GitHub Release (`.github/workflows/release.yml`). Pull requests and
other branches run the tests on Windows and Linux (`ci.yml`).
