# Time-cut patch

This is little_exif 0.6.23 from crates.io, upstream commit 2d6d73acea033630a1cf0056d23b9da81e0638c8.
Time-cut uses quick-xml 0.42 with the upstream parsing fix for RUSTSEC-2026-0194.
The XMP cleanup routine uses the new API, skips entire EXIF subtrees, and rejects
malformed input before replacing the PNG metadata chunk.
The crate retains the upstream MIT or Apache-2.0 license.
