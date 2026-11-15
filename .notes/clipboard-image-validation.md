# Clipboard image metadata validation

## What
Reject malformed incoming image clip metadata before sending PULL. Update feature status to distinguish implemented image sync from unsupported non-image file clips.

## How
`src/clipboard.rs::image_clip_id` now requires an explicit size that converts safely to `usize`, a non-zero size no larger than the phone's 10 MiB limit, and `attr == 128`. Added regression cases for missing size, missing file attribute and zero size. README and TODO now reflect image-sync implementation and real-phone verification gap.

## Why
Previous defaults treated missing size as zero and missing `attr` as a regular file, allowing incomplete metadata to trigger an unnecessary transfer request. Narrow validation rejects unknown metadata rather than assuming a safe file.

## Verification
`cargo test -p smartconnect-fex` passes (27 tests). Root workspace tests remain blocked because system GStreamer development files are unavailable; no real phone test performed.
