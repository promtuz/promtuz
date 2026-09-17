# Android license notices

Each Android variant generates its offline license catalog before assets are
merged. No runtime network access or extra app dependency is involved.

Gradle resolves the variant's runtime artifacts and their POMs, including parent
POMs that supply inherited license declarations. `generate.py` reads those
artifacts, including AAR `classes.jar`, runtime `libs/*.jar`, and Google's indexed
third-party notices. License names come from the published metadata, not a blanket
assumption about the publisher.

The native inventory reads Cargo's `compiler-artifact` messages from the actual
`buildRustCore` invocation for both Android targets. Cargo emits these records for
cached builds too. `cargo-artifacts.py` forwards Cargo's stdout unchanged while
recording it, because cargo-ndk otherwise consumes those messages. Build scripts
receive the actual Cargo binary, and a failed build cannot publish its partial
inventory. Only libraries compiled for the Android targets are included;
host-only build tools and procedural macros are excluded. Package metadata comes
from the exact manifests recorded by Cargo, including workspace-inherited fields.
This avoids resolving or downloading unrelated server/development dependencies
with `cargo metadata` or `cargo tree`. The inventory retains all target libraries,
even if individual functions are later removed by linking or shrinking.

Crate LICENSE, NOTICE, COPYING and COPYRIGHT files are included recursively to
retain bundled code notices, including AWS-LC, ring and their embedded libraries.
The SQLite public-domain notice comes from its bundled amalgamation. SQLCipher's
unused alternative backend is excluded. The generator does not infer undocumented
native dependencies from opaque binaries; native dependencies omitted by an AAR
need reviewed supplements below.

Texts are deduplicated by content hash. The list loads only the small index;
library details and individual notice texts load separately off the UI thread.
Unknown dependencies without any notice or supported published license fail the
build with their coordinates. After upgrading a manually supplemented dependency,
review its notices and update the pinned mapping in `generate.py`.

An initial build needs access to the configured Maven repositories for license
POMs as well as the usual dependency artifacts. Once cached, normal offline
builds work. Python 3.11 or newer is required and uses only its standard library.
To regenerate explicitly:

```sh
cd android
./gradlew :app:generateDebugLicenseAssets --rerun-tasks
```

## Supplemental source notices

These upstream texts fill omissions in published dependency archives:

| File | Upstream source |
| --- | --- |
| `Apache-2.0.txt`, `MPL-2.0.txt`, `CC0-1.0.txt` | [SPDX license-list-data v3.27.0](https://github.com/spdx/license-list-data/tree/v3.27.0/text) |
| `libavif.txt` | [libavif 841110fd LICENSE](https://github.com/AOMediaCodec/libavif/blob/841110fd/LICENSE), including its libyuv notice |
| `dav1d.txt` | [dav1d 1.5.1 COPYING](https://github.com/videolan/dav1d/blob/1.5.1/COPYING); pinned by libavif's `ext/dav1d_android.sh` |
| `android-cpufeatures.txt` | First comment in NDK 29.0.14206865 `sources/android/cpufeatures/cpu-features.c`; this library is linked by libavif's Android JNI CMake target |
| `libffi.txt` | [JNA 5.19.0 native/libffi/LICENSE](https://github.com/java-native-access/jna/blob/5.19.0/native/libffi/LICENSE) |
| `openmls.txt` | [OpenMLS 47dbedec LICENSE](https://github.com/openmls/openmls/blob/47dbedecad0c1fd8eb5368d582250ebfcc1e1ce6/LICENSE); source revision recorded in the published crate's `.cargo_vcs_info.json` |
| `capturable.txt` | [Capturable v3.0.1 LICENSE](https://github.com/PatilShreyas/Capturable/blob/v3.0.1/LICENSE) |
| `protobuf.txt` | [protobuf v28.2 LICENSE](https://github.com/protocolbuffers/protobuf/blob/v28.2/LICENSE); DataStore 1.1.7's shaded `RuntimeVersion` declares Java version 4.28.2 |
