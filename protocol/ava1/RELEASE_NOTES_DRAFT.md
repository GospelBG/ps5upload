# Release notes draft: the AVA1 cutover

DRAFT. The numbers in "Measured performance" are filled in after the hardware
pass (Task 22). Do not publish this file as is.

## One port, one protocol

ps5upload now talks to your PS5 over a single connection type on a single
port: **9120**. The old transfer and management ports (9113 and 9114) are
gone. Everything the app does with the console, from uploads to browsing,
installs, hardware readings and copy, goes through that one port.

**Firewall.** If you allow ports by hand, you now need only **9120** (the
helper) and **9021** (your ELF loader, which is not part of ps5upload).
You can remove any rule for 9113 and 9114.

## Pairing

The first time a computer talks to a console it has to be trusted.

- A helper that the app launched pairs by itself. You type nothing.
- A helper you loaded some other way (another loader, a hand-sent ELF)
  shows a **6-digit code** on the console. Confirm it in the app once and the
  computer is remembered.

Several computers can pair with one console. Two engines that share one
identity file (a copied data folder) would kick each other off, so give each
its own data folder; the engine warns you when it sees this.

## Everything over AVA1, including management

File browsing, copy, move, delete, process and hardware readings, package
installs and the other management actions now run on the same encrypted
connection as transfers. Long jobs (deleting a big folder, a console copy)
return at once and report progress, so one slow job no longer blocks the rest.

## 7z and RAR uploads resume

7z and RAR archives are unpacked on the fly and sent straight to the console;
nothing is written to your disk. If the connection drops or the helper
restarts, the upload **resumes**: the app unpacks the archive again but skips
what the console already has (the screen shows a "skipping" phase with its own
progress) and carries on from there.

- If the archive changed on disk since the first attempt, the upload starts
  over instead of mixing two versions.
- A RAR whose file order changed is refused on resume rather than spliced.

## Zip files

- **Zip entries larger than 256 MiB** now stream like any other file. They
  used to be set aside; a single 1.5 GB entry is fine.
- A zip entry is checked as it is read; a corrupt entry fails the upload with
  an error instead of arriving damaged.
- **Zip download resume.** Downloading a folder as a zip now resumes after a
  dropped connection from where it stopped. The default archive is stored
  (uncompressed), which is what makes resume possible; the optional compressed
  zip cannot resume and restarts from zero.

## Faster tiny files, and "Finishing on the console..."

Small files are now written to a log first and made permanent in a few
batches, instead of paying a slow disk flush for every file. Uploads of
many small files stop being limited by the drive's per-file flush rate.

The result: the upload is reported done when the data is safely logged, and
the console may need a few seconds more to put the last files in their final
places. During that time the app shows **"Finishing on the console..."**.
Nothing is lost if the helper stops in that window: it picks the files up
again on its next start.

## Encryption and verification

Every AVA1 connection is encrypted and authenticated (Noise handshake,
ChaCha20-Poly1305), and file contents are verified with BLAKE3. Only paired
computers can talk to the helper. As before, run the app on a network you
trust.

## Update the helper from the app

If the console is running an older helper (v5.41 or earlier), the app says so
and offers **Update the helper**. One click shuts the old helper down, waits
for its ports to close, and sends the new one; because the app sends it,
there is no pairing code. If the old helper does not exit, the app asks you
to restart the console and try again. Helper restarts need about 60 seconds
between them.

If you launch the new helper yourself while an old one is running, it takes
the old one over.

## Environment variables

| Now | Was | Notes |
|---|---|---|
| `PS5UPLOAD_BANDWIDTH_MBPS` | `FTX2_BANDWIDTH_MBPS` | Upload speed cap in MB/s. Unset or 0 means no cap. |
| `PS5UPLOAD_ZIP_RAM_THRESHOLD_MB` | `FTX2_ZIP_RAM_THRESHOLD_MB` | Accepted, but does nothing: archives stream. |
| `PS5UPLOAD_ARCHIVE_STAGE_MB` | `FTX2_ARCHIVE_STAGE_MB` | Accepted, but does nothing: archives stream. |

The old `FTX2_*` names keep working for this release and print a deprecation
line in the engine log. They stop working in the next release.
`PS5UPLOAD_TRANSFER` is gone; there is only one transfer path.

## Measured performance (filled after the hardware pass)

To fill from CUTOVER.md section 4, re-run at the release commit on both
consoles (Pro `/data`, `/mnt/usb0`, `/mnt/ext1`; Phat `/data`, `/mnt/usb0`,
`/mnt/ext0`). Pass criterion: AVA1 at least 90% of the recorded old-protocol
numbers on the large-file and tiny-upload rows; any row that misses is listed
here.

| Row | AVA1 | Old protocol (last measured) |
|---|---|---|
| 4 GiB upload (MB/s), per drive | TBD | TBD |
| 2,000 tiny files upload (files/s), per drive | TBD | TBD |
| 2,000 tiny files download (files/s), per drive | TBD | TBD |
| Console copy, 2,000 tiny files (files/s) | TBD | TBD |
| Resume 4 GiB after a helper kill (MB/s) | TBD | TBD |
| 4 GiB with the link cut every 10 s (MB/s) | TBD | not run |
| 223,000-file upload (files/s) | TBD | failed |
| Minecraft, Worms, Minecraft Legends (s, MB/s) | TBD | TBD |
| 7z and RAR 5 GB, solid and non-solid: upload, kill at 50%, resume, time to first byte | TBD | n/a |
| Zip with a 1.5 GB entry: upload and resume | TBD | n/a |
| Zip download, link drop at 50%: bytes resumed | TBD | n/a |
| `fs.read` of a 2 MiB `param.json` | TBD | TBD |
| Phat usb0 large file (explain or document the drive) | TBD | TBD |

## Known limitations

- The hardware rows above have not been measured on the final build yet. Earlier
  runs showed large files 5-8% below the old protocol and a slower tiny-file
  download on some drives; see the table when it is filled.
- RAR file times come from a DOS timestamp read through your computer's time
  zone. If the time zone changes between an upload and its resume, or a stamp
  lands within a day of "now", that upload restarts instead of resuming. Rare,
  and it only costs a restart.
- The compressed (deflate) zip download cannot resume.
