# Vendored native sources

| Project | Revision | Source | Archive SHA-256 | License |
| --- | --- | --- | --- | --- |
| libavif | v1.4.2 | [release](https://github.com/AOMediaCodec/libavif/archive/refs/tags/v1.4.2.tar.gz) | `2b645287340ba5a631d268b551dc2d72bd73ac33335962dd36dcdb6d8366921d` | BSD-2-Clause |
| dav1d | 1.5.4 | [release](https://github.com/videolan/dav1d/archive/refs/tags/1.5.4.tar.gz) | `a1d5b63d2d38ec9bd03acf643caa51fa22edd1e89c5a109c4807717216bbec07` | BSD-2-Clause |
| rav1e C ABI | 0.7.1 | [crate](https://crates.io/crates/rav1e/0.7.1) | `cd87ce80a7665b1cce111f8a16c1f3929f6547ce91ade6addf4ec86a8dda5ce9` | BSD-2-Clause |

`vendor/rav1e/rav1e.h` is the minimal C ABI declaration consumed by libavif,
checked against the pinned `rav1e` 0.7.1 source. Its implementation is linked
from that Cargo dependency, not from a host package.
