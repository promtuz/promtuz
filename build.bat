@echo off

cargo ndk -t arm64-v8a -t x86_64 -o ../android/app/src/main/jniLibs -P 33 build --release