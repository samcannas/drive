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
