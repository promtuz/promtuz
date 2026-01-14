# libcore

Contains core business logic for app.

Due to the nature of rust it can be easily ported for almost any platform like Android, iOS, Windows, macOS & Linux.

Currently `libcore` is only oriented around **Android** and **JNI** but that will change in future.

Because `libcore` contains almost all necessary code that connects to the decentralized network, any platform's GUI or even TUI use this `core` lib using FFI or other inter-op technology, of course that platform has to be declared in `libcore` along with providing ways to **secure** the credentials.
