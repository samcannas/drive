# Transfers

How Drive moves bytes to uloz.to: resumable uploads, what the transfer list shows, and the
user's controls. Code: `crates/drive-core/src/ulozto/chunked.rs` (wire protocol),
`remote/resumable.rs` (shared types), `remote/crypt.rs` (`EncryptedSource`) and
`engine/transfers.rs` (list, reasons, controls).

## Resumable uploads

Files of 16 MiB and up (measured as stored, i.e. encrypted) use uloz.to's chunked upload API
(<https://uloz.to/upload-resumable-api-beta>). Smaller files go up in a single request, which
costs fewer API calls and has nothing worth resuming.

1. **Prepare.** One pass over the file computes the whole-file MD5 and SHA-256 (for rclone's
   metadata), the CRC32 (for the server's own check) and an MD5 per chunk.
2. **Batch.** `POST /v6/upload/link` returns a batch (`private_slug`), a signature and the
   resumable host. When resuming, the same call with the `private_slug` renews the signature,
   and `POST {host}/v1/chunked-file-upload/{slug}` extends the batch. Long uploads renew the
   signature before it expires.
3. **Register** the file: size, chunk size (4, 32 or 128 MiB depending on file size) and CRC32.
4. **Chunks** are `POST {host}/v1/chunked-file-upload/{slug}/1/{n}`. The server reports which
   chunks are missing, so only those are sent. Several chunks of one file are in flight at
   once (see *Parallelism*).
5. **Assembly.** `GET …/status` until `finished`. The server assembles the file, checks the
   CRC32 (`bad-crc32` otherwise) and returns the file's slug.
6. **Verify and commit.** `GET /v5/file/{slug}/hash-verify` confirms our MD5 before anything
   becomes visible. Then the name, folder and rclone metadata are set and the batch is
   committed, just like a single-request upload.

Everything needed to continue (the encryption nonce, batch, chunk size, hashes and confirmed
chunk count) is kept as a `ResumeState`. The engine saves it in the `upload_session` table
after every step. After a dropped connection, a pause, a crash or a reboot, the next attempt
asks the server what's missing and sends only that. A session is reused only while the
local file's size and modification time are unchanged and the fresh preparation pass gives
the same MD5.

### Safety rules

- **A nonce is never reused for different data.** The crypt layer saves its file nonce so a
  resumed upload produces byte-identical ciphertext. Every chunk is re-hashed right before it
  is sent and compared with the preparation pass. If the file changed, the upload stops and
  the session is dropped, so the next attempt starts with a fresh nonce. Non-resumable
  uploads never save a nonce.
- **Nothing unverified becomes visible.** The server's CRC32 and our MD5 must both pass before
  the commit. On any integrity failure the partial upload is deleted from the resumable host.
- **No duplicates.** If the commit succeeded but its reply was lost (or Drive crashed right
  after), the next attempt sees the already-assembled file, finds it committed and adopts
  it. Until then the planner treats that remote file as part of the pending upload, never as
  a conflict.
- **Fallback.** If the chunked API is refused (missing or erroring), Drive uses single-request
  uploads for an hour.

### Parallelism

`AdaptiveLimit` caps chunk requests in flight across all uploads, using AIMD:
- after a streak of successes the cap goes up by one, as long as throughput rose by at least
  5% since the last increase; otherwise it steps back and stops probing for 10 minutes;
- a 429, a 5xx or a stalled chunk halves it.

Chunks are held in memory before sending (to verify them), so each upload is also limited
to about 160 MiB of chunks.

Every chunk has a stall watchdog: no bytes for 2 minutes, or no reply for 10 minutes after
the last byte. Each chunk is retried up to 4 times with backoff before the engine's own
retry (with backoff) takes over.

## The transfer list

`Call::Transfers` and `Event::Transfers` (pushed about once a second while anything is
pending) list running transfers first, then waiting ones, then paused and scheduled ones.
Each item has a plain-language reason when it isn't moving: open in another app, offline,
signed out, waiting for its folder, still changing, retrying in 30 s (with the cause),
paused, waiting for tonight.

Rates are smoothed per transfer (EWMA, sampled every second). The ETA is the bytes left of
everything not paused or scheduled divided by the current upload rate. For queued uploads
with a session, `bytes_done` shows what is already safely on the server.

## Controls

`Call::SetTransferMode { id, mode }`, also `drive transfer pause|resume|tonight|now PATH`:

| Mode | Effect |
|---|---|
| `paused` | Stops at the next chunk; confirmed chunks stay on the server. |
| `auto` | Resume and follow the settings. |
| `tonight` | Wait for the overnight window. |
| `now` | Upload now even if the overnight window applies; goes to the front of the queue. |

The mode returns to `auto` once the file is uploaded.

**Overnight uploads** (Settings → Network, or `drive settings night-uploads=on
night-start=01:00 night-end=07:00 night-min-mb=200`): while the window is closed, files at
least that large wait. Small files and edits still sync right away.

## Automatic upload speed

A bulk upload fills the large buffer most home routers and modems have on the upstream
link, and every other packet leaving the house then waits behind it: calls stutter and
pages load slowly although nothing is "broken". `engine/adaptive.rs` slows uploads down
while (and only while) that happens. It is on by default (`adaptive_upload`, Settings →
Network → *Keep your connection responsive*, `drive settings auto-upload-speed=off`) and
applies below any manual upload limit.

**Measuring.** About once a second while uploading, the service times a TCP handshake to
`apis.uloz.to:443` (nothing is requested). The handshake's SYN waits in the same queue as
our data, so `handshake − base` is the queuing delay, where the base is the fastest
handshake of the last ten minutes. The first five measurements come 200 ms apart so the
base is known before uploads start. This is LEDBAT's idea (RFC 6817) applied as a rate
limit.

**Never slowing down without cause.**

1. Nothing happens unless the spike-filtered delay (lowest sample over ~3 s) stays above
   120 ms for 6 s while uploads send at least 16 KB/s.
2. Then a *check*: uploads pause (up to 8 s). Only if the delay falls back near the base
   (or at least halves, for deep OS socket buffers) is a limit set, at 80 % of the rate
   before. If the delay stays, something else causes it (another device, a full download
   link, a VPN, Wi-Fi): uploads stay at full speed and no check runs for 5 minutes, then
   10, 20, 30. A check that sees no change at all ends after 6 s.
3. While limited, the median of the last three samples steers the limit towards 60 ms of
   queue: above it, the link is full, so the rate that filled it is its speed and the
   limit goes to that rate minus enough to drain the excess within 3 s (an integral term
   corrects the estimate); below it, the limit grows by up to 8 %/s but never more than
   20 % past what uploads achieve.
4. The limit is removed as soon as it stops mattering: uploads finish (30 s idle), uploads
   achieve 15 % less than it allows with a short queue for 20 s (small files, or the
   limit drifted above the link's speed), or measurements stop (fail open). A connection
   that gets faster needs no release: the limit follows it up (gently near the rate where
   the queue last built, 1 %/s, as TCP Cubic does). A delay
   that stays high for 20 s despite the limit triggers a new check.
5. The throttle's automatic limit and pause lapse on their own after 10 s unless renewed,
   so a stopped controller can never leave uploads slowed or paused.
6. The base is only right if some measurement saw an empty queue. When the service starts
   with uploads already running (a resumed chunked upload keeps the queue full for hours),
   every sample is inflated and the queue would stay hidden. So when no measurement was
   taken while our uploads weren't running, uploads pause once (at most ~6 s) to
   *calibrate*: a round trip that drops by more than 120 ms reveals our queue and limiting
   starts as after a check; otherwise the base was right and is trusted for 30 minutes (any
   pause, check or calibration, counts as a fresh measurement; a calibration that found
   someone else's queue is retried then).

Every transition is logged (`automatic upload speed: …` in the service log) with the
measurements behind it; the transfer list and `drive transfers` show the limit while it
applies.

**Tests.** `adaptive_tests.rs` checks each rule, then runs a simulated network (bottleneck
link and buffer, OS socket buffer, the token bucket, other traffic, delay from elsewhere,
noise) against an uncontrolled baseline: a managed queue, Wi-Fi jitter, someone else's
upload, a busy download link, a VPN, small uploads, failing or flickering measurements must never be slowed
(a restart mid-upload on a healthy link costs one pause of ≤ 7 s); a bloated uplink, a link that gets slower and
then faster, a video call alongside, a very slow link, a restart or switch-on mid-upload and a
40-minute upload must get a short queue at near full speed; and a sweep of 120 random links (32 KB/s – 12.5 MB/s, 5–600 ms base, 5 ms –
2 s buffers, half of them starting mid-upload) must hold the rules for every one: never limited where there is nothing to
fix, ≥ 97 % of the uncontrolled speed where the delay isn't ours, ≥ 80 % speed and half
the delay where it is. `tests/adaptive_netem.rs` runs the real controller, probe and throttle over real TCP on a
loopback the kernel shapes into an 8 Mbit/s link (CI job *Upload speed on shaped links*: on
changes to the controller, throttle or these tests, weekly, and on demand). On a bloated
link (one-second buffer) our upload's queue must shrink to a quarter (it measured 496 → 30
ms, p90 506 → 94 ms) at ≥ 85 % of the speed (98 %); on a healthy link (short buffer) nothing
may be limited or paused after settling and speed must stay ≥ 97 % (100 %); with someone
else's unpaced upload filling the bloated queue ours must never be limited (97 % of its
speed).
